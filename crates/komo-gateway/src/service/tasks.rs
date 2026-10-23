//! 后台任务（`docs/background-tasks.md`）：`TaskSpawner` 的生产实现，以及任务收尾时把
//! 结果交回派它的那个会话。
//!
//! 建任务复用已有原语，一步都不新造：`SessionId::new_at` → `session::ensure_owned`（写死
//! `kind = Task`、`agent_id` = 派它的会话的 Agent）→ `ledgers.open`（落目录、`events.jsonl`）
//! → `session::set_title_if_empty`（标题先写死，`accept_input` 的"首行当标题"规则才不会
//! 盖掉它）→ `GatewayState::submit_task`（冻结身份、受理输入、挂看客；`task` 给的模型与
//! 结果契约随快照冻结）。
//!
//! **它自己需要一份 `Arc<GatewayState>`**（建会话、`submit` 都要），所以只能在那份状态
//! 造出来**之后**才能接上——与 `Dispatcher` / `GatewayState::inbound` 同一个"先有整份
//! 状态才能自己引用自己"的接法（见 `GatewayState::wire_task_spawner`）。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::events::EventPayload;
use komo_kernel::traits::{SpawnError, TaskSpawner};
use komo_kernel::types::agent::{ForkOrigin, MemoryMode, RunSnapshot};
use komo_kernel::types::chat::ChannelPeer;
use komo_kernel::types::chat::{DeliveryTarget, Outbound};
use komo_kernel::types::delegate::DelegateContract;
use komo_kernel::types::ids::{RequestKey, RunId, SessionId, ToolCallId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::status::RunState;
use komo_kernel::types::task::{self, FollowOutcome, TaskHandle, TaskOptions, TaskSpec};
use komo_kernel::types::turn::{ForkPrefix, ReplayMessage, Role, ToolResultForModel};
use komo_store::models::SessionKind;
use komo_store::repos::session;

use super::state::GatewayState;

/// fork 任务第一条 Run 的快照：父 Run 的身份照抄（能力面含 `task`、模型、档位、工作目录、
/// 投影），系统提示由冻结的前缀给出，所以指向父会话 `payloads/` 的引用一概不带——它们在
/// 任务会话的目录里读不到，也用不上。记忆不召回：召回的段在父的系统提示里已经有了，再按
/// 任务会话召回一次，前缀就不是同一个了（§2b）。
fn fork_snapshot(parent: RunSnapshot, origin: ForkOrigin) -> RunSnapshot {
    RunSnapshot {
        instructions_ref: None,
        project_files_ref: None,
        project_files_update_ref: None,
        skills_ref: None,
        memory_mode: MemoryMode::Off,
        direct_command: None,
        contract: None,
        fork: Some(origin),
        ..parent
    }
}

/// 任务会话的 `origin`：`task:{parent_session}`。`task_id` 的解析与收尾时"结果交回谁"
/// 都按它——任务属于哪个会话不需要新列。
pub fn task_origin(parent: &SessionId) -> String {
    format!("task:{parent}")
}

/// [`task_origin`] 的反解：一个会话的 `origin` 是不是 `task:{parent}` 的样子，是就给出
/// `parent`。`komo session list` 与 [`report_to_parent`] 都用它。
pub fn home_of(origin: &str) -> Option<SessionId> {
    origin.strip_prefix("task:").map(SessionId::from_raw)
}

/// 同一个会话名下、短号能匹配上 `task_id` 的任务会话。
async fn candidates_for(
    db: &komo_store::Db,
    parent: &SessionId,
    task_id: &str,
) -> Result<Vec<komo_store::repos::session::SessionRecord>, komo_kernel::traits::StoreError> {
    let origin = task_origin(parent);
    let records = session::list(db, false).await?;
    Ok(records
        .into_iter()
        .filter(|record| {
            record.kind == SessionKind::Task
                && record.origin == origin
                && task::matches_short_id(&record.session, task_id)
        })
        .collect())
}

pub struct GatewayTaskSpawner {
    state: Arc<GatewayState>,
}

/// fork 任务第一条输入的开头（`docs/background-tasks.md` §2b）：紧跟在父会话的对话与补的
/// 结果之后。放在输入里而不是系统提示里——系统提示是父会话那份，改一个字缓存就不中。
pub const FORK_NOTICE: &str = "（以上是派你的那个会话到此刻为止的对话。你是从那里分出来的后台任务：\
只做下面这件事。派你的会话同时还在跑，同一个工作目录里的文件可能正在变——不要接着做它没做完的事，\
不要改它正在改的东西，也不要再派任务。做完就停，你最后的回复会作为那次 task 调用的结果交回给它。）";

/// 派出这个 fork 的那次调用在 fork 里的结果。
const FORK_CALL_RESULT: &str = "这次调用派出的就是你（fork）。派你的会话在继续它自己的事，你最后的回复会作为这次调用的结果交回给它。";

/// 同一轮里排在派出那次之后、还没执行的调用在 fork 里的结果。
const FORK_LATER_CALL_RESULT: &str = "派你的会话在派出你之后才执行这次调用，你看不到它的结果。";

/// 给父会话最后一轮里还没有结果的调用补上结果（`docs/background-tasks.md` §2b）：派出这个
/// fork 的那次，以及同一轮里排在它后面、还没执行的那些（编排调用是屏障，排在前面的都已经
/// 收尾）。补的结果只进 fork 的视图，不进任何账。没有它们，provider 看到一个没有输出的调用
/// 直接 400。
fn close_open_calls(messages: &mut Vec<ReplayMessage>, call: &ToolCallId) -> Result<(), String> {
    let Some(turn) = messages
        .iter()
        .rposition(|message| message.role == Role::Assistant && !message.tool_calls.is_empty())
    else {
        return Err("派任务的会话最后一轮里没有调用".into());
    };
    let answered: Vec<&ToolCallId> = messages[turn + 1..]
        .iter()
        .flat_map(|message| &message.tool_results)
        .map(|result| &result.call_id)
        .collect();
    let calls = &messages[turn].tool_calls;
    if !calls.iter().any(|request| &request.call_id == call) {
        return Err("派出这个 fork 的调用不在派任务的会话最后一轮里".into());
    }
    let results: Vec<ToolResultForModel> = calls
        .iter()
        .filter(|request| !answered.contains(&&request.call_id))
        .map(|request| ToolResultForModel {
            provider_call_id: request.provider_call_id.clone(),
            call_id: request.call_id.clone(),
            content: if &request.call_id == call {
                FORK_CALL_RESULT
            } else {
                FORK_LATER_CALL_RESULT
            }
            .into(),
            is_error: false,
            images: Vec::new(),
        })
        .collect();
    let seq = messages
        .last()
        .map(|message| message.seq)
        .unwrap_or_default();
    messages.push(ReplayMessage {
        role: Role::Tool,
        seq,
        text: None,
        tool_calls: Vec::new(),
        tool_results: results,
        provider_blocks: None,
    });
    Ok(())
}

impl GatewayTaskSpawner {
    pub fn new(state: Arc<GatewayState>) -> Arc<Self> {
        Arc::new(Self { state })
    }

    /// 派它的那条 Run 所在的会话，以及该投给哪个渠道对端（`runs.peer`，
    /// `ChannelPeer::parse` 是它的反解）。不管会话是什么种类：查进度也走这里。
    async fn caller_of(
        &self,
        from: &RunId,
    ) -> Result<(session::SessionRecord, Option<ChannelPeer>), SpawnError> {
        let run = komo_store::repos::runs::get(&self.state.db, from)
            .await
            .map_err(|error| SpawnError::Failed(format!("读不到派它的那条 Run：{error}")))?
            .ok_or_else(|| SpawnError::Failed(format!("派它的那条 Run {from} 不在账本里")))?;
        let caller = session::get(&self.state.db, &run.session)
            .await
            .map_err(|error| SpawnError::Failed(format!("读不到派它的那个会话：{error}")))?
            .ok_or_else(|| SpawnError::Failed(format!("派它的会话 {} 不在库里", run.session)))?;
        let peer = run.peer.as_deref().and_then(ChannelPeer::parse);
        Ok((caller, peer))
    }

    /// fork 的种子：父会话这条 Run 下一次请求的样子（切口处的调用已补上结果）与它冻结的身份。
    async fn fork_seed(
        &self,
        parent: &SessionId,
        from: &RunId,
        call: &ToolCallId,
    ) -> Result<(ForkPrefix, RunSnapshot), SpawnError> {
        let (context, snapshot) = self
            .state
            .segments
            .fork_view(parent, from)
            .await
            .map_err(|reason| SpawnError::Failed(format!("fork 不出来：{reason}")))?;
        let mut messages = context.messages;
        close_open_calls(&mut messages, call)
            .map_err(|reason| SpawnError::Failed(format!("fork 不出来：{reason}")))?;
        Ok((
            ForkPrefix {
                system_prompt: context.system_prompt,
                messages,
            },
            snapshot,
        ))
    }

    /// 派它的那条 Run 属于哪个会话（任务会话的 parent）、该投给哪个渠道对端。
    ///
    /// **任务会话里不能再派任务**：结果只交回一层，再往下派就成了一棵没人看得见的树。
    async fn parent_of(
        &self,
        from: &RunId,
    ) -> Result<(session::SessionRecord, Option<ChannelPeer>), SpawnError> {
        let (parent, peer) = self.caller_of(from).await?;
        if parent.kind == SessionKind::Task {
            return Err(SpawnError::Failed(
                "这里已经是一个后台任务了，不能再派任务：自己把这件事做完".into(),
            ));
        }
        Ok((parent, peer))
    }

    /// `task_id` 在 `parent` 名下对应的那一个任务会话；没有 = `UnknownTask`，短号撞了 =
    /// `Ambiguous`（`follow` 与 `status` 共用）。
    async fn resolve_task(
        &self,
        parent: &SessionId,
        task_id: &str,
    ) -> Result<session::SessionRecord, SpawnError> {
        let mut candidates = candidates_for(&self.state.db, parent, task_id)
            .await
            .map_err(|error| SpawnError::Failed(format!("任务会话列不出来：{error}")))?;
        if candidates.is_empty() {
            return Err(SpawnError::UnknownTask(format!(
                "#{task_id} 不是这个会话派出去的任务"
            )));
        }
        if candidates.len() > 1 {
            let listed = candidates
                .iter()
                .map(|record| format!("#{}", task::extended_short_id(&record.session)))
                .collect::<Vec<_>>()
                .join("、");
            return Err(SpawnError::Ambiguous(format!(
                "#{task_id} 有歧义，候选：{listed}"
            )));
        }
        Ok(candidates.remove(0))
    }

    /// 任务 Run 用的模型（`docs/komo_bot.md` §4）：给了 alias 就按当前配置解析；没给就是
    /// `inherited`（接着做时那个任务上一条 Run 的，新派时 `None` = 交给受理按 Agent 的默认）。
    /// 给了 `effort` 就按选定的那个模型的档位校验（§13.3）——只给 `effort` 时 `prepare`
    /// 不知道选定的是哪个模型，在这里校验。
    fn task_model(
        &self,
        options: &TaskOptions,
        inherited: Option<ModelConfig>,
        agent_id: &str,
    ) -> Result<Option<ModelConfig>, SpawnError> {
        let config = self.state.snapshot();
        let mut model = match &options.model {
            Some(alias) => Some(config.model_catalog.completion(alias).cloned().ok_or_else(
                || {
                    let aliases = config
                        .model_catalog
                        .completions()
                        .map(|(alias, _)| alias)
                        .collect::<Vec<_>>()
                        .join("、");
                    SpawnError::Failed(format!("model `{alias}` 不是可选的模型；可选值：{aliases}"))
                },
            )?),
            None => inherited,
        };
        if let Some(effort) = &options.effort {
            let mut chosen = model.unwrap_or_else(|| self.state.default_model_of(agent_id));
            chosen.effort = Some(effort.clone());
            self.state
                .caps
                .check(&chosen)
                .map_err(|problem| SpawnError::Failed(format!("effort 立不住：{problem}")))?;
            model = Some(chosen);
        }
        Ok(model)
    }
}

#[async_trait]
impl TaskSpawner for GatewayTaskSpawner {
    async fn spawn(
        &self,
        from: &RunId,
        request_key: RequestKey,
        spec: TaskSpec,
    ) -> Result<TaskHandle, SpawnError> {
        let (parent, origin_peer) = self.parent_of(from).await?;
        // fork 的种子先取：取不到就不该留下一个空的任务会话。
        let fork = match &spec.fork {
            Some(call) => Some(self.fork_seed(&parent.session, from, call).await?),
            None => None,
        };
        let now = self.state.clock.now();
        let task_session = SessionId::new_at(now);
        let origin = task_origin(&parent.session);
        let jsonl_path = format!("sessions/{task_session}/events.jsonl");
        let worker = self
            .state
            .agent_of_session(&parent.session)
            .await
            .map_err(|error| {
                SpawnError::Failed(format!("读不出派它的会话归哪个 Agent：{error}"))
            })?;
        // 模型先解析：解析不出来就不该留下一个空的任务会话。fork 沿用父 Run 的那个。
        let model = match &fork {
            Some((_, snapshot)) => Some(snapshot.model.clone()),
            None => self.task_model(&spec.options, None, &worker)?,
        };

        // 会话行**写死**在受理任何输入之前：`kind = Task`、`agent_id` 与派它的会话相同；
        // 幂等——同一个 id 重放只会原样返回已有的那一行。
        session::ensure_owned(
            &self.state.db,
            &task_session,
            &origin,
            &jsonl_path,
            &worker,
            SessionKind::Task,
        )
        .await
        .map_err(|error| SpawnError::Failed(format!("任务会话建不出来：{error}")))?;

        // 目录 + `events.jsonl`：`ledgers.open` 内部的 `ensure_in` 是幂等的，行已经在
        // 就原样返回，不会把上面写好的 `kind` / `agent_id` 改回默认值。
        self.state
            .ledgers
            .open(&task_session, &origin)
            .await
            .map_err(|error| SpawnError::Failed(format!("任务会话目录开不出来：{error}")))?;

        // 标题在第一条输入**之前**写死：`accept_input` 的"首行当标题"只填空的会话
        // （`docs/komo_bot.md` §8.5），先写上就不会被那条规则盖掉。
        session::set_title_if_empty(&self.state.db, &task_session, &spec.title)
            .await
            .map_err(|error| SpawnError::Failed(format!("标题记不上：{error}")))?;

        let response = match (fork, &spec.fork) {
            (Some((prefix, parent_snapshot)), Some(call)) => {
                // 前缀按值存进**任务会话自己的** `payloads/`：父会话删掉之后它照样读得到（§2b）。
                let prefix = komo_store::PayloadStore::new(
                    self.state.routed.ledgers().paths_for(&task_session),
                )
                .put_json(&prefix, None)
                .await
                .map_err(|error| SpawnError::Failed(format!("fork 的前缀存不下来：{error}")))?;
                let snapshot = fork_snapshot(
                    parent_snapshot,
                    ForkOrigin {
                        parent_session: parent.session.clone(),
                        parent_run: from.clone(),
                        call: call.clone(),
                        prefix,
                    },
                );
                self.state
                    .submit_fork(
                        &task_session,
                        request_key,
                        format!("{FORK_NOTICE}\n\n{}", spec.task),
                        origin_peer,
                        snapshot,
                        spec.options.contract,
                    )
                    .await
            }
            _ => {
                self.state
                    .submit_task(
                        &task_session,
                        request_key,
                        spec.task,
                        origin_peer,
                        model,
                        spec.options.contract,
                        false,
                    )
                    .await
            }
        }
        .map_err(|error| SpawnError::Failed(format!("提交第一条输入失败：{error}")))?;

        Ok(TaskHandle {
            session: response.session,
            short_id: task::short_id(&task_session),
            title: spec.title,
        })
    }

    async fn follow(
        &self,
        from: &RunId,
        request_key: RequestKey,
        task_id: &str,
        text: &str,
        options: TaskOptions,
    ) -> Result<FollowOutcome, SpawnError> {
        let (parent, origin_peer) = self.parent_of(from).await?;

        let target = self.resolve_task(&parent.session, task_id).await?;

        // 没给 model 就沿用这个任务上一条 Run 的（连同它的 effort）。
        let inherited = match &options.model {
            Some(_) => None,
            None => komo_store::repos::runs::list_for_session(&self.state.db, &target.session)
                .await
                .map_err(|error| SpawnError::Failed(format!("读不出这个任务的 Run：{error}")))?
                .pop()
                .map(|record| record.model),
        };
        let model = self.task_model(&options, inherited, &target.agent_id)?;

        // 追问以插话受理：任务正忙时在它下一次请求模型之前并进去，不排到整条 Run 后面。
        // 带契约的照旧排队——并入沿用宿主的快照，新契约会丢（§6）。
        let interject = options.contract.is_none();
        let response = self
            .state
            .submit_task(
                &target.session,
                request_key,
                text.to_string(),
                origin_peer,
                model,
                options.contract,
                interject,
            )
            .await
            .map_err(|error| SpawnError::Failed(format!("提交失败：{error}")))?;

        Ok(FollowOutcome {
            session: response.session,
            short_id: task::short_id(&target.session),
            queued_behind: response.state == RunState::Waiting,
        })
    }

    async fn status(&self, from: &RunId, task_id: &str) -> Result<task::TaskProgress, SpawnError> {
        let (caller, _) = self.caller_of(from).await?;
        let target = self.resolve_task(&caller.session, task_id).await?;
        let events = super::compaction::read_events(self.state.routed.as_ref(), &target.session)
            .await
            .map_err(|error| SpawnError::Failed(format!("读不出任务会话的日志：{error}")))?;
        let mut run = komo_kernel::fold::progress_of(&events);
        if let Some(run) = run.as_mut() {
            let paths = self.state.routed.ledgers().paths_for(&target.session);
            for call in &mut run.running {
                if let Some(attempt) = &call.attempt {
                    call.output_tail = output_tail(&paths, &run.run, &call.call, attempt).await;
                }
            }
        }
        Ok(task::TaskProgress {
            short_id: task::short_id(&target.session),
            title: target.title,
            run,
        })
    }
}

/// 读到的输出尾部至多这么多字节：只要最后几行，不必把可能很大的流读完。
const TAIL_READ_BYTES: u64 = 2048;
/// 给模型的尾部至多这么多个非空行。
const TAIL_LINES: usize = 8;

/// 一次还没出结果的尝试的输出尾部：优先 stdout，空则 stderr；运行中读 `.partial`，
/// 已发布（但结果还没落 JSONL）就读正式文件。读不到就是 `None`，不是错误。
async fn output_tail(
    paths: &komo_store::SessionPaths,
    run: &RunId,
    call: &komo_kernel::types::ids::ToolCallId,
    attempt: &komo_kernel::types::ids::AttemptId,
) -> Option<String> {
    let dir = paths
        .tool_output()
        .join(run.as_str())
        .join(call.as_str())
        .join(attempt.as_str());
    for name in ["stdout.txt", "stderr.txt"] {
        for file in [format!("{name}.partial"), name.to_string()] {
            if let Some(tail) = read_tail(&dir.join(file)).await {
                return Some(tail);
            }
        }
    }
    None
}

/// 文件末尾至多 [`TAIL_READ_BYTES`] 字节里最后 [`TAIL_LINES`] 个非空行；从中间截开时
/// 丢掉开头不完整的行。全空白 = `None`。
async fn read_tail(path: &std::path::Path) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    let start = len.saturating_sub(TAIL_READ_BYTES);
    file.seek(std::io::SeekFrom::Start(start)).await.ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_READ_BYTES)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // 起点落在一行（或一个多字节字符）中间：开头那段残的不要；整段没有换行就只去掉残字符。
    let text = match (start > 0, text.find('\n')) {
        (true, Some(newline)) => &text[newline + 1..],
        (true, None) => text.trim_start_matches('\u{FFFD}'),
        (false, _) => &text[..],
    };
    let lines = text.lines();
    let kept = lines
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let tail = &kept[kept.len().saturating_sub(TAIL_LINES)..];
    (!tail.is_empty()).then(|| tail.join("\n"))
}

/// 一条任务 Run 收尾时怎么样了（[`report_to_parent`] 的标题那一半）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    Completed,
    NotCompleted,
}

/// 任务会话里一条 Run 收尾了：把结果当作**一条内部输入**交回派它的会话，由那边的模型
/// 转告用户（hermes 的 async delegation 同一个做法）。
///
/// 结果不直接投给渠道：派任务的那个会话要看得见结果，用户再问"进度怎么样"时它才答得上，
/// 而不是只看得见一句"已派出"。请求键 `task-result:{run}` 让同一条 Run 的结果只交回
/// 一次——看客与重启补挂各收尾一遍也只受理一条。
///
/// 交回的输入带插话标记（§6）：派它的会话正在跑、而且是同一个对端时，在一轮收尾之后并进
/// 那条 Run，不另开一条；不带模型 / effort，按派它的会话自己的默认。任务 Run 带着结果契约
/// 时，用同一份校验器复验它的最后回复，结论写进交回的输入——不合契约也照样交回。
///
/// 返回 `false` = 这不是任务会话，调用方照常把结果投给来源。交回失败（派它的会话已经
/// 删掉了、受理出错）时退回直接投给来源，结果不能因此丢掉。
pub async fn report_to_parent(
    state: &Arc<GatewayState>,
    task_session: &SessionId,
    run: &RunId,
    peer: Option<&ChannelPeer>,
    outcome: TaskOutcome,
    text: &str,
) -> bool {
    let record = match session::get(&state.db, task_session).await {
        Ok(Some(record)) if record.kind == SessionKind::Task => record,
        Ok(_) => return false,
        Err(error) => {
            tracing::warn!(%error, session = %task_session, "读不出这个会话，结果照常投给来源");
            return false;
        }
    };
    let Some(parent) = home_of(&record.origin) else {
        return false;
    };
    let headline = match outcome {
        TaskOutcome::Completed => "已完成",
        TaskOutcome::NotCompleted => "没有完成",
    };
    let verdict = match (outcome, contract_of(state, task_session, run).await) {
        (TaskOutcome::Completed, Some(contract)) => {
            format!("{}\n\n", contract_verdict(&contract, text))
        }
        _ => String::new(),
    };
    let input = format!(
        "[后台任务 #{short}「{title}」{headline}]\n\n{text}\n\n{verdict}\
         （这是后台任务交回的结果，不是用户发来的消息；用户看不到上面这段，请把结论转告用户。）",
        short = task::short_id(task_session),
        title = record.title,
    );
    let key = RequestKey::new(format!("task-result:{run}"));
    match state
        .submit_interjecting(&parent, key, input, peer.cloned(), None)
        .await
    {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, %run, %parent, "后台任务的结果交不回派它的会话，直接投给来源");
            if let Some(peer) = peer
                && let Err(error) = state
                    .notifier
                    .log()
                    .deliver(
                        &DeliveryTarget::to_peer(peer.clone()),
                        Outbound::RunFinished {
                            session: task_session.clone(),
                            run: run.clone(),
                            summary: text.to_string(),
                        },
                    )
                    .await
            {
                tracing::warn!(%error, %run, "回复投不出去");
            }
            true
        }
    }
}

/// 这条任务 Run 受理时冻结的结果契约（`run.accepted` 的快照，§4）。读不出来就当没有。
async fn contract_of(
    state: &GatewayState,
    task_session: &SessionId,
    run: &RunId,
) -> Option<DelegateContract> {
    let events = match super::compaction::read_events(state.routed.as_ref(), task_session).await {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(%error, %run, "读不出任务会话的日志，交回时不复验契约");
            return None;
        }
    };
    events.into_iter().find_map(|event| match event.payload {
        EventPayload::RunAccepted(body) if event.run.as_ref() == Some(run) => {
            body.snapshot.and_then(|snapshot| snapshot.contract)
        }
        _ => None,
    })
}

/// 复验结论：与旧委派的父侧收尾同一份取 JSON + 校验（`executor::check_contract`）。
fn contract_verdict(contract: &DelegateContract, text: &str) -> String {
    match komo_runtime::executor::check_contract(contract, text) {
        Ok((_, validation)) if validation.ignored.is_empty() => "（结果契约复验：符合。）".into(),
        Ok((_, validation)) => format!(
            "（结果契约复验：符合；这些关键字没有检查：{}。）",
            validation.ignored.join("、")
        ),
        Err(report) => format!("（结果契约复验：不符合——{report}。）"),
    }
}

/// 任务会话里那些"需要你判断"的消息前面挂的标签：`后台任务 #2a9c「查空调」`。不是任务
/// 会话就是 `None`。操作者在聊天里同时看着几个任务时，靠它分清是哪一个在问。
pub async fn label_of(state: &GatewayState, task_session: &SessionId) -> Option<String> {
    let record = session::get(&state.db, task_session).await.ok()??;
    if record.kind != SessionKind::Task {
        return None;
    }
    Some(format!(
        "后台任务 #{}「{}」",
        task::short_id(task_session),
        record.title
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_tail_keeps_the_last_non_empty_lines_of_a_big_partial_file() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("stdout.txt.partial");
        let mut body = String::new();
        for n in 0..500 {
            body.push_str(&format!("行 {n}\n\n"));
        }
        std::fs::write(&path, &body).expect("写文件");

        let tail = read_tail(&path).await.expect("有尾部");
        let lines = tail.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), TAIL_LINES, "{tail}");
        assert_eq!(lines.last(), Some(&"行 499"));
        assert!(lines.iter().all(|line| line.starts_with("行 ")), "{tail}");
    }

    #[tokio::test]
    async fn the_tail_cut_inside_a_multibyte_char_starts_on_a_whole_line() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("stdout.txt.partial");
        std::fs::write(&path, "好".repeat(3000)).expect("写文件");

        let tail = read_tail(&path).await.expect("有尾部");
        assert!(!tail.contains('\u{FFFD}'), "不该有半个字符");
    }

    #[tokio::test]
    async fn a_missing_or_blank_output_has_no_tail() {
        let dir = tempfile::tempdir().expect("临时目录");
        assert_eq!(read_tail(&dir.path().join("nope")).await, None);
        let blank = dir.path().join("blank");
        std::fs::write(&blank, "\n  \n").expect("写文件");
        assert_eq!(read_tail(&blank).await, None);
    }

    /// fork 切口（`docs/background-tasks.md` §2b）：同一轮里排在派出那次前面、已经收尾的调用
    /// 不补；派出那次补"派出的就是你"，排在它后面的补"你看不到结果"，按调用顺序。
    #[test]
    fn the_cut_closes_the_fork_call_and_the_ones_after_it() {
        use komo_kernel::types::ids::{Seq, ToolCallId};
        use komo_kernel::types::turn::{ReplayMessage, Role, ToolCallRequest, ToolResultForModel};

        let call = |id: &str| ToolCallRequest {
            call_id: ToolCallId::from_raw(id),
            provider_call_id: format!("p-{id}"),
            name: "x".into(),
            arguments: serde_json::json!({}),
            arguments_ref: None,
        };
        let message = |role: Role, seq: u64| ReplayMessage {
            role,
            seq: Seq(seq),
            text: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            provider_blocks: None,
        };
        let mut turn = message(Role::Assistant, 2);
        turn.tool_calls = vec![call("read"), call("fork"), call("later")];
        let mut done = message(Role::Tool, 3);
        done.tool_results = vec![ToolResultForModel {
            provider_call_id: "p-read".into(),
            call_id: ToolCallId::from_raw("read"),
            content: "读到了".into(),
            is_error: false,
            images: Vec::new(),
        }];
        let mut messages = vec![message(Role::User, 1), turn, done];

        super::close_open_calls(&mut messages, &ToolCallId::from_raw("fork")).unwrap();
        let closed = messages.last().unwrap();
        assert_eq!(closed.role, Role::Tool);
        let ids: Vec<&str> = closed
            .tool_results
            .iter()
            .map(|result| result.provider_call_id.as_str())
            .collect();
        assert_eq!(ids, ["p-fork", "p-later"]);
        assert!(closed.tool_results[0].content.contains("派出的就是你"));
        assert!(closed.tool_results[1].content.contains("看不到它的结果"));

        let mut unrelated = vec![message(Role::User, 1)];
        assert!(super::close_open_calls(&mut unrelated, &ToolCallId::from_raw("fork")).is_err());
    }
}
