//! 停住的运行在对话里开口（`docs/redesign.md` P14、§2.1），以及用对话里的回复答复它们。
//!
//! 一个 Run 进入需要人答复的状态——任务出错（faulted）、连续被崩溃打断后停住、无人值守的 Run 要调对外工具
//! （confirm）——时，往它所在的对话投一句话（[`DurableCore::say_in`]）：说清停在哪、要你决定什么、能怎么答，
//! `data` 里带 `handle`。落在哪段对话见 [`DurableCore::route_of`]：支线里的 Run 投那段支线，主对话里的、
//! 由主对话派出的任务会话（沿 `parent_session` 往上）投主对话，定时触发的会话投 [`DurableCore::cron_route`]（目标绑了支线就是那段支线）。
//!
//! # 什么时候投
//!
//! [`spawn`] 在引擎开始调度**之前**订阅提交，之后写下的 faulted / confirm 任务一个不漏；启动时再把
//! 仍未答复的全部回放一遍（上一代可能崩在投之前）。重复由关联键挡掉（`intervention:{kind}:{handle}`，见
//! [`say`](super::say) 的判重），所以回放与订阅对同一条各投一次也只有一句。崩溃停住的任务是调度器的派生
//! 状态、不进提交，只在启动时投一次；键里带它起跑的时刻，放行后再次停住算新的一句。
//!
//! # 怎么答
//!
//! 操作者在对话里回一句话，由这段对话里的模型调 `komo` 工具的 `intervention.answer`
//! （[`DurableCore::answer_from_chat`]）：
//!
//! - **只有操作者亲自发消息开始的 Run 能答**：触发器起的 Run（`cron.input`、监听事件、任务交回，判定同
//!   `docs/confirm.md` §3）、任务会话与定时会话里一律拒绝；
//! - 只能答指向**这段对话**的介入；
//! - 答复走现有的答复路径（[`DurableCore::answer_intervention`]）：不认的答复报错（confirm 只认
//!   `approve` / `reject`），答过的返回原结论。

use std::collections::HashSet;
use std::sync::Arc;

use komo_kernel::durable::{
    SessionKind, SessionLifecycle, SessionRecord, StorageWrite, legacy_ids,
};
use komo_kernel::protocol::http::{
    ActivityWaiting, InterventionAnswerResponse, InterventionDetail, InterventionSummary,
    InterventionVerdict,
};
use komo_kernel::traits::{GatewayError, Shutdown};
use komo_kernel::types::ids::{RunId, SessionId};
use komo_runtime::durable::harness::is_trigger_input;
use serde_json::json;
use tokio::sync::mpsc;

use super::super::DurableCore;
use super::super::interventions::confirms::confirming;
use super::super::interventions::fault_message;
use super::super::sessions::SessionError;
use super::Route;
use super::say::{Said, Say, SaySource};
use crate::service::state::GatewayState;

/// 轮询停机旗子的间隔（`Shutdown` 只是个标志，不能 await）。
const SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(250);
/// 话里引用的对外调用参数最多这么多字符。
const ARGUMENTS_CHARS: usize = 300;

/// 一个答复用人话怎么说（话里列「你可以回复……」）。
fn verdict_phrase(verdict: InterventionVerdict) -> &'static str {
    match verdict {
        InterventionVerdict::Approve => "「执行」",
        InterventionVerdict::Reject => "「不执行」",
        InterventionVerdict::Resolve => "「重试」",
        InterventionVerdict::Abandon => "「放弃」",
        InterventionVerdict::Satisfied => "「已经做完了」",
        InterventionVerdict::NotPerformed => "「没做，重新做」",
    }
}

fn choices(verdicts: &[InterventionVerdict]) -> String {
    let list = verdicts
        .iter()
        .map(|verdict| verdict_phrase(*verdict))
        .collect::<Vec<_>>()
        .join("、");
    format!("你可以回复：{list}。")
}

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(chars).collect();
    cut.push('…');
    cut
}

/// 给界面看的话：停在哪、要你决定什么、能怎么答。`origin` 是它从哪来（「定时任务「早报」」之类；就在这段
/// 对话里时为空）。
fn spoken(detail: &InterventionDetail, origin: &str) -> String {
    let lead = if origin.is_empty() {
        "这段对话里".to_owned()
    } else {
        origin.to_owned()
    };
    match detail {
        InterventionDetail::Confirm {
            summary,
            tool,
            arguments,
            deadline,
        } => {
            let minutes = ((*deadline - summary.created_at).whole_minutes()).max(1);
            let wait = if minutes % 60 == 0 {
                format!("{} 小时", minutes / 60)
            } else {
                format!("{minutes} 分钟")
            };
            format!(
                "{lead}有一个对外动作等你确认：要调用 {tool}，参数 {}。{}{wait}内不回复就不执行。",
                clip(&arguments.to_string(), ARGUMENTS_CHARS),
                choices(&summary.verdicts),
            )
        }
        InterventionDetail::Blocked { summary, reason } => {
            let stuck = if summary.verdicts.contains(&InterventionVerdict::Resolve) {
                "停住了"
            } else {
                "出错了"
            };
            format!(
                "{lead}有一步{stuck}：{reason}。{}",
                choices(&summary.verdicts)
            )
        }
        InterventionDetail::Verify {
            summary, reason, ..
        } => format!(
            "{lead}有一次调用结果不明：{reason}。{}",
            choices(&summary.verdicts)
        ),
    }
}

/// 只给模型看的附注：内部句柄与能答什么，以及什么时候才能提交。
fn model_note(summary: &InterventionSummary) -> String {
    let verdicts = summary
        .verdicts
        .iter()
        .map(|verdict| verdict.as_str())
        .collect::<Vec<_>>()
        .join(" / ");
    format!(
        "（内部信息，别念给操作者。待答复事项：handle={}，类型 {}，可答 {verdicts}。\
         只有操作者在回复里明确回答了上面这个问题，才用 komo 工具的 intervention.answer（handle、verdict）提交；\
         没说清就先问清楚，不要替操作者做决定。）",
        summary.handle,
        summary.kind.as_str(),
    )
}

fn summary_of(detail: &InterventionDetail) -> &InterventionSummary {
    match detail {
        InterventionDetail::Verify { summary, .. }
        | InterventionDetail::Blocked { summary, .. }
        | InterventionDetail::Confirm { summary, .. } => summary,
    }
}

/// 关联键：同一条介入只投一次。崩溃停住的（能 `resolve`）带上它起跑的时刻：放行后再停算新的一句。
fn key_of(summary: &InterventionSummary) -> String {
    let base = format!("intervention:{}:{}", summary.kind.as_str(), summary.handle);
    if summary.verdicts.contains(&InterventionVerdict::Resolve) {
        format!("{base}:{}", summary.created_at.unix_timestamp())
    } else {
        base
    }
}

impl DurableCore {
    /// 还没答复的介入所在的会话（每条一个，同一会话可重复）：faulted、崩溃停住、待确认。
    pub(super) async fn pending_intervention_sessions(
        &self,
    ) -> Result<Vec<SessionRecord>, SessionError> {
        let mut sessions: Vec<SessionRecord> = Vec::new();
        sessions.extend(
            self.faulted(None)
                .await?
                .into_iter()
                .map(|found| found.session),
        );
        sessions.extend(
            self.stopped(None)
                .await?
                .into_iter()
                .map(|found| found.session),
        );
        sessions.extend(
            self.confirming(None)
                .await?
                .into_iter()
                .map(|found| found.session),
        );
        Ok(sessions)
    }

    /// 一条介入落话的那段对话（会话行）。
    async fn intervention_chat(&self, session: &str) -> Result<SessionRecord, SessionError> {
        let record = self.session(session).await?;
        let route = self.route_of(&record).await?;
        self.resolve_route(route).await
    }

    /// 它从哪来的一小段话（落在别的会话里时才有）。
    async fn origin_of(&self, session: &SessionRecord, chat: &SessionRecord) -> String {
        match session.kind {
            SessionKind::Cron => match session.title.as_deref() {
                Some(name) if !name.is_empty() => format!("定时任务「{name}」里"),
                _ => "定时任务里".to_owned(),
            },
            SessionKind::Task => self
                .task_label(&session.id)
                .await
                .map(|label| format!("{label}里"))
                .unwrap_or_else(|| "后台任务里".to_owned()),
            _ if session.id == chat.id => String::new(),
            _ => match session.title.as_deref() {
                Some(title) if !title.is_empty() => format!("会话「{title}」里"),
                _ => "另一个会话里".to_owned(),
            },
        }
    }

    /// 把这条介入投到它所在的对话。已经答复（清单里没有了）返回 `None`；已经投过返回 [`Said::Duplicate`]。
    pub async fn announce_intervention(&self, handle: &str) -> Result<Option<Said>, SessionError> {
        let Some(detail) = self.intervention(handle).await? else {
            return Ok(None);
        };
        let summary = summary_of(&detail);
        let session = self.session(summary.session.as_str()).await?;
        if !matches!(
            session.lifecycle,
            SessionLifecycle::Active | SessionLifecycle::Closing
        ) {
            return Ok(None);
        }
        let chat = self.intervention_chat(&session.id).await?;
        let say = Say {
            text: spoken(&detail, &self.origin_of(&session, &chat).await),
            model_note: Some(model_note(summary)),
            source: SaySource::Intervention,
            key: key_of(summary),
            data: json!({
                "handle": summary.handle,
                "interventionKind": summary.kind.as_str(),
                "session": summary.session,
            }),
        };
        match self.say_in(&chat.id, say.clone()).await {
            // 目标对话刚好不收输入了：落回主对话。
            Err(SessionError::NotOpen(_)) => {
                let main = self.resolve_route(Route::Main).await?;
                Ok(Some(self.say_in(&main.id, say).await?))
            }
            other => other.map(Some),
        }
    }

    /// 还没答复的介入：会话 id → 问话投在哪段对话、介入的句柄。目标活动 / 运行历史标「在等你」用
    /// （P16）；同一会话有几条时留第一条。
    pub async fn pending_asks(
        &self,
    ) -> Result<std::collections::HashMap<String, ActivityWaiting>, SessionError> {
        let mut out = std::collections::HashMap::new();
        for summary in self.interventions(&Default::default()).await? {
            let session = summary.session.as_str().to_owned();
            if out.contains_key(&session) {
                continue;
            }
            // 会话读不出来（刚被清掉）的不标：这里只是个提示，不让它拖垮整张详情。
            let Ok(chat) = self.intervention_chat(&session).await else {
                continue;
            };
            out.insert(
                session,
                ActivityWaiting {
                    chat: SessionId::from_raw(chat.id),
                    handle: summary.handle.clone(),
                },
            );
        }
        Ok(out)
    }

    /// 指向 `chat` 这段对话、还没答复的介入。
    pub async fn interventions_for_chat(
        &self,
        chat: &str,
    ) -> Result<Vec<InterventionSummary>, SessionError> {
        let mut out = Vec::new();
        for summary in self.interventions(&Default::default()).await? {
            let target = self.intervention_chat(summary.session.as_str()).await?;
            if target.id == chat {
                out.push(summary);
            }
        }
        Ok(out)
    }

    /// 这个 Run 是不是操作者亲自发消息开始的：第一条输入放置成的 entry 不是触发器的
    /// （`cron.input` / 监听 / 任务交回）。读不出来按"不是"算。
    pub async fn run_started_by_operator(&self, run: &RunId) -> bool {
        use komo_kernel::durable::{InputState, SubmissionKind};
        let Some(first) = legacy_ids::submission_of(run) else {
            return false;
        };
        let Ok(Some(submission)) = self.storage.submission(first).await else {
            return false;
        };
        let entry = match submission.kind {
            SubmissionKind::Input {
                state: InputState::Placed { entry } | InputState::Done { entry, .. },
            } => entry,
            _ => return false,
        };
        match self.storage.entry(entry).await {
            Ok(Some(stored)) => !is_trigger_input(&stored.entry.kind),
            _ => false,
        }
    }

    /// `komo` 工具的 `intervention.answer`：见模块文档。`session` / `run` 是调用它的会话与 Run。
    pub async fn answer_from_chat(
        &self,
        session: &str,
        run: &RunId,
        handle: &str,
        verdict: InterventionVerdict,
    ) -> Result<InterventionAnswerResponse, ChatAnswerError> {
        let record = self
            .session(session)
            .await
            .map_err(ChatAnswerError::from_session)?;
        if !matches!(record.kind, SessionKind::Main | SessionKind::Normal) {
            return Err(ChatAnswerError::Denied(
                "任务会话与定时任务会话不能答复：答复只能由操作者在对话里亲自回答".into(),
            ));
        }
        if !self.run_started_by_operator(run).await {
            return Err(ChatAnswerError::Denied(
                "这一轮不是操作者发消息开始的（定时、监听、任务交回触发的 Run 不能替操作者答复）；\
                 把要问的话留在对话里，等操作者回复后再答"
                    .into(),
            ));
        }
        // 还在等答复的，必须指向这段对话；已经答过的（清单里没有了）走原路径，返回原结论。
        if let Some(detail) = self
            .intervention(handle)
            .await
            .map_err(ChatAnswerError::from_session)?
        {
            let target = self
                .intervention_chat(summary_of(&detail).session.as_str())
                .await
                .map_err(ChatAnswerError::from_session)?;
            if target.id != session {
                return Err(ChatAnswerError::Invalid(format!(
                    "{handle} 不是在这段对话里等你答复的（intervention.list 能看到这段对话里等着的）"
                )));
            }
        }
        self.answer_intervention(handle, verdict, format!("chat:{session}"))
            .await
            .map_err(ChatAnswerError::from_gateway)
    }
}

/// [`DurableCore::answer_from_chat`] 的失败：工具层把它们映射成"被拒绝 / 参数不对 / 失败"。
#[derive(Debug, thiserror::Error)]
pub enum ChatAnswerError {
    #[error("{0}")]
    Denied(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
}

impl ChatAnswerError {
    fn from_session(error: SessionError) -> Self {
        match error {
            SessionError::NotFound(_) | SessionError::Invalid(_) => {
                Self::Invalid(error.to_string())
            }
            other => Self::Failed(other.to_string()),
        }
    }

    fn from_gateway(error: GatewayError) -> Self {
        match error {
            GatewayError::NotFound { .. } | GatewayError::InvalidRequest(_) => {
                Self::Invalid(error.to_string())
            }
            other => Self::Failed(other.to_string()),
        }
    }
}

// ------------------------------------------------------------------- 订阅

/// 起介入的投递（`service::start` 调，**在 `core.resume()` 之前**）：订阅是同步建好的，之后出现的任务一个
/// 不漏；任务里先回放仍未答复的，再等新的。
pub fn spawn(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    let Ok(core) = state.core() else { return };
    let (sender, mut receiver) = mpsc::unbounded_channel::<u64>();
    let _ = core.engine.line().subscribe_commits(move |publication| {
        // 回调在提交线上同步执行：只把任务号送出去。
        for write in &publication.writes {
            if let StorageWrite::Task(record) = write
                && (fault_message(record).is_some() || confirming(record))
            {
                let _ = sender.send(record.id.get());
            }
        }
    });
    let weak = Arc::downgrade(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut seen: HashSet<u64> = HashSet::new();
        if let Some(state) = weak.upgrade()
            && let Ok(core) = state.core()
        {
            replay(core, &mut seen).await;
        }
        loop {
            let received = tokio::time::timeout(SHUTDOWN_POLL, receiver.recv()).await;
            if shutdown.is_cancelled() {
                return;
            }
            let id = match received {
                Err(_) => continue,
                Ok(Some(id)) => id,
                Ok(None) => return,
            };
            // 同一个任务的检查点每拍都会写：投过的不再过一遍（判重本来也会挡，这里省掉提交）。
            if !seen.insert(id) {
                continue;
            }
            let Some(state) = weak.upgrade() else { return };
            let Ok(core) = state.core() else { return };
            if let Err(error) = core.announce_intervention(&id.to_string()).await {
                // 没投成：下次这个任务再写时重试。
                seen.remove(&id);
                tracing::warn!(%error, handle = id, "停住的运行没能投到对话里");
            }
        }
    });
}

/// 启动时回放：全部仍未答复的介入各过一遍。
async fn replay(core: &crate::service::durable::DurableCore, seen: &mut HashSet<u64>) {
    let mut handles: Vec<String> = Vec::new();
    match core.faulted(None).await {
        Ok(all) => handles.extend(all.iter().map(|found| found.handle())),
        Err(error) => tracing::warn!(%error, "读不出 faulted 任务，启动时的补投没做成"),
    }
    match core.stopped(None).await {
        Ok(all) => handles.extend(all.iter().map(|found| found.handle())),
        Err(error) => tracing::warn!(%error, "读不出停住的任务，启动时的补投没做成"),
    }
    match core.confirming(None).await {
        Ok(all) => handles.extend(all.iter().map(|found| found.handle())),
        Err(error) => tracing::warn!(%error, "读不出待确认的调用，启动时的补投没做成"),
    }
    for handle in handles {
        match core.announce_intervention(&handle).await {
            Ok(_) => {
                if let Ok(id) = handle.parse() {
                    seen.insert(id);
                }
            }
            Err(error) => tracing::warn!(%error, %handle, "停住的运行没能投到对话里"),
        }
    }
}
