//! 后台任务（`task`，`docs/background-tasks.md`）：把耗时的事派给独立的**任务会话**在
//! 后台跑。2026-10-16 之前是 `dispatch` / `follow` 两个工具，旧计划照旧解码。
//!
//! 值类型放在这里，理由与 [`super::delegate`] 一样：编排操作（`Operation::Task`，以及只为
//! 旧计划保留的 `Operation::Dispatch` / `Operation::Follow`）要能落进
//! [`super::plan::ExecutionPlan`]，而 kernel 不依赖 runtime / gateway。短号那几个纯函数
//! 也放这里——回执、`task_id` 的解析与 `komo session list` 共用同一份实现，撞号与解析
//! 不能有第二种口径。

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::delegate::DelegateContract;
use super::ids::{AttemptId, RunId, SessionId, ToolCallId};
use super::model::Effort;
use super::status::RunState;

/// 建一个任务要交代的事：自包含的正文 + 给人看的标题（§4.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    /// 交给任务会话的自包含任务；它就是那条 Run 的第一条输入正文。
    pub task: String,
    /// 给人看的标题（≤ 30 字，建会话时写死，不因为后续消息改动）。
    pub title: String,
    #[serde(default)]
    pub options: TaskOptions,
}

/// 任务那条 Run 的模型、档位与结果契约（`docs/komo_bot.md` §4）。新派与接着做共用。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOptions {
    /// `[model.<alias>]` 的 alias。`None`：新任务用 Agent 的默认，接着做沿用那个任务上一条
    /// Run 的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// 结果契约：任务会话的提示带上它，交回之前用同一份校验器复验最后回复。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<DelegateContract>,
}

/// 新派一个任务成功后的句柄：模型把它念给操作者听。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskHandle {
    pub session: SessionId,
    /// 任务短号（[`short_id`]）。
    pub short_id: String,
    pub title: String,
}

/// 接着做（带 `task_id`）成功后的句柄。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowOutcome {
    pub session: SessionId,
    pub short_id: String,
    /// 提交的这一刻，那个任务会话已经有一条没跑完的 Run——模型据此措辞
    /// "正在跑，这句排在它后面"而不是"已转给"（§4.1）。
    pub queued_behind: bool,
}

/// 一个任务现在跑到哪了（`task` 只带 `task_id` 的查询，`docs/background-tasks.md`）。
///
/// 只读快照：由任务会话的事件折出来（[`crate::fold::progress_of`]），Gateway 再补上正在
/// 跑的调用的输出尾部。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskProgress {
    pub short_id: String,
    pub title: String,
    /// `None`：任务会话里还没有 Run。
    pub run: Option<RunProgress>,
}

/// 任务会话里最近一条 Run 的进度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunProgress {
    pub run: RunId,
    pub state: RunState,
    /// `run.accepted` 的时间。
    #[serde(with = "time::serde::rfc3339")]
    pub accepted_at: OffsetDateTime,
    /// 这条 Run 最近一条事件的时间。
    #[serde(with = "time::serde::rfc3339")]
    pub last_at: OffsetDateTime,
    pub rounds: u32,
    pub calls: u32,
    /// 还没出结果的调用（`Planned` / `Started`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub running: Vec<RunningCall>,
    /// 最近一条带正文的 `message.assistant`（截断）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 终态时的最终回复（截断）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    /// `failed` / `abandoned` 的原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `waiting` 时在等什么，已渲染成一句话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
}

/// 一次还没出结果的调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningCall {
    pub call: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<AttemptId>,
    pub tool: String,
    /// 计划参数的一行摘要。
    pub summary: String,
    /// `tool.started` 的时间；还没 started 就是 `None`。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<OffsetDateTime>,
    /// 输出尾部。kernel 没有 I/O，不填；Gateway 读 `.partial` 的尾部补上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
}

impl TaskProgress {
    /// 给模型看的中文快照。`now` 由调用方从 [`crate::traits::Clock`] 取。
    pub fn render(&self, now: OffsetDateTime) -> String {
        let head = format!("#{}「{}」", self.short_id, self.title);
        let Some(run) = &self.run else {
            return format!("{head}：还没有开始跑");
        };
        let mut out = String::new();
        match run.state {
            RunState::Accepted | RunState::Queued => {
                let _ = write!(
                    out,
                    "{head}：排队中，已等 {}，还没开始跑",
                    duration_words(now - run.accepted_at)
                );
            }
            RunState::Running => {
                let _ = write!(
                    out,
                    "{head}：运行中，已跑 {}，第 {} 轮，调用 {} 次；上次活动 {}前",
                    duration_words(now - run.accepted_at),
                    run.rounds,
                    run.calls,
                    duration_words(now - run.last_at),
                );
            }
            RunState::Waiting => {
                let _ = write!(
                    out,
                    "{head}：暂停等待中，已 {}，第 {} 轮，调用 {} 次",
                    duration_words(now - run.accepted_at),
                    run.rounds,
                    run.calls,
                );
                if let Some(on) = &run.waiting_on {
                    let _ = write!(out, "；在等：{on}");
                }
            }
            RunState::Completed
            | RunState::Failed
            | RunState::Cancelled
            | RunState::Abandoned
            | RunState::Absorbed => {
                let word = match run.state {
                    RunState::Completed => "已完成",
                    RunState::Failed => "已失败",
                    RunState::Cancelled => "已取消",
                    RunState::Abandoned => "已放弃",
                    _ => "已并入另一条 Run",
                };
                let _ = write!(
                    out,
                    "{head}：{word}，用时 {}，共 {} 轮、调用 {} 次",
                    duration_words(run.last_at - run.accepted_at),
                    run.rounds,
                    run.calls,
                );
                if let Some(reason) = &run.reason {
                    let _ = write!(out, "\n原因：{reason}");
                }
                if let Some(text) = &run.final_message {
                    let _ = write!(out, "\n最终回复：{text}");
                }
                out.push_str("\n结果会（或已）作为任务回报交回这个会话，不必再来查。");
                return out;
            }
        }
        for call in &run.running {
            out.push_str("\n正在执行：");
            out.push_str(&call.tool);
            match call.started_at {
                Some(at) => {
                    let _ = write!(out, "，已 {}", duration_words(now - at));
                }
                None => out.push_str("，还没真正开始"),
            }
            if !call.summary.is_empty() {
                let _ = write!(out, "  `{}`", call.summary);
            }
            if let Some(tail) = call.output_tail.as_deref().filter(|t| !t.trim().is_empty()) {
                out.push_str("\n  最近输出：");
                for line in tail.trim_end().lines() {
                    out.push_str("\n  ");
                    out.push_str(line);
                }
            }
        }
        if let Some(note) = &run.note {
            let _ = write!(out, "\n最近一句：{note}");
        }
        out
    }
}

/// 一段时间的人话：不到一分钟说秒，不到一小时说分秒，再长说时分。负数按 0。
fn duration_words(span: time::Duration) -> String {
    let secs = span.whole_seconds().max(0);
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{} 时 {} 分", secs / 3600, secs % 3600 / 60)
    }
}

/// 任务短号：任务会话 id 末 4 位（UUIDv7 的随机段），回执与 `task_id` 解析都用它
/// （§4.3）。
pub fn short_id(session: &SessionId) -> String {
    suffix(session, 4)
}

/// 撞号时延长到的那个长度（§4.3）。
pub fn extended_short_id(session: &SessionId) -> String {
    suffix(session, 6)
}

fn suffix(session: &SessionId, len: usize) -> String {
    let raw = session.as_str();
    let take = raw.len().saturating_sub(len);
    raw[take..].to_string()
}

/// 这个会话的 id 是不是以 `want` 结尾（大小写不敏感）。空串永远不匹配——一个空的
/// `task_id` 不该被解成"随便哪个都行"。
pub fn matches_short_id(session: &SessionId, want: &str) -> bool {
    let want = want.trim();
    if want.is_empty() {
        return false;
    }
    session
        .as_str()
        .to_ascii_lowercase()
        .ends_with(&want.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn session(raw: &str) -> SessionId {
        SessionId::from_raw(raw)
    }

    #[test]
    fn short_id_is_the_last_four_characters() {
        let id = session("0190f000-aaaa-7000-8000-0000003f2a9c");
        assert_eq!(short_id(&id), "2a9c");
        assert_eq!(extended_short_id(&id), "3f2a9c");
    }

    #[test]
    fn matching_is_case_insensitive_and_rejects_empty() {
        let id = session("0190f000-aaaa-7000-8000-0000003f2a9c");
        assert!(matches_short_id(&id, "2a9c"));
        assert!(matches_short_id(&id, "2A9C"));
        assert!(matches_short_id(&id, "3f2a9c"));
        assert!(!matches_short_id(&id, "  "));
        assert!(!matches_short_id(&id, "zzzz"));
    }

    fn progress(state: RunState) -> RunProgress {
        RunProgress {
            run: RunId::from_raw("run-1"),
            state,
            accepted_at: datetime!(2026-10-19 08:00:00 UTC),
            last_at: datetime!(2026-10-19 08:47:04 UTC),
            rounds: 31,
            calls: 52,
            running: vec![],
            note: None,
            final_message: None,
            reason: None,
            waiting_on: None,
        }
    }

    fn snapshot(run: Option<RunProgress>) -> TaskProgress {
        TaskProgress {
            short_id: "2a9c".into(),
            title: "抓数据".into(),
            run,
        }
    }

    #[test]
    fn a_running_snapshot_renders_elapsed_the_running_call_and_its_output_tail() {
        let mut run = progress(RunState::Running);
        run.running.push(RunningCall {
            call: ToolCallId::from_raw("c1"),
            attempt: None,
            tool: "bash".into(),
            summary: "cd /tmp/x && for i in …".into(),
            started_at: Some(datetime!(2026-10-19 08:40:32 UTC)),
            output_tail: Some("t=380 done=4\nrunning=8\n".into()),
        });
        run.note = Some("在等第二批".into());
        let text = snapshot(Some(run)).render(datetime!(2026-10-19 08:47:12 UTC));
        assert!(
            text.contains("#2a9c「抓数据」：运行中，已跑 47 分 12 秒"),
            "{text}"
        );
        assert!(
            text.contains("第 31 轮，调用 52 次；上次活动 8 秒前"),
            "{text}"
        );
        assert!(
            text.contains("正在执行：bash，已 6 分 40 秒  `cd /tmp/x && for i in …`"),
            "{text}"
        );
        assert!(
            text.contains("\n  最近输出：\n  t=380 done=4\n  running=8"),
            "{text}"
        );
        assert!(text.contains("最近一句：在等第二批"), "{text}");
    }

    #[test]
    fn a_completed_snapshot_says_how_long_it_took_and_that_the_result_is_handed_back() {
        let mut run = progress(RunState::Completed);
        run.final_message = Some("都跑完了".into());
        let text = snapshot(Some(run)).render(datetime!(2026-10-19 09:00:00 UTC));
        assert!(text.contains("已完成，用时 47 分 4 秒"), "{text}");
        assert!(text.contains("最终回复：都跑完了"), "{text}");
        assert!(text.contains("交回"), "{text}");
    }

    #[test]
    fn a_failed_snapshot_carries_the_reason() {
        let mut run = progress(RunState::Failed);
        run.reason = Some("模型一直 429".into());
        let text = snapshot(Some(run)).render(datetime!(2026-10-19 09:00:00 UTC));
        assert!(
            text.contains("已失败") && text.contains("原因：模型一直 429"),
            "{text}"
        );
    }

    #[test]
    fn queued_waiting_and_empty_snapshots_each_say_so() {
        let now = datetime!(2026-10-19 08:10:00 UTC);
        assert!(
            snapshot(Some(progress(RunState::Queued)))
                .render(now)
                .contains("排队中")
        );
        let mut waiting = progress(RunState::Waiting);
        waiting.waiting_on = Some("更早的一条 Run（r0）跑完".into());
        let text = snapshot(Some(waiting)).render(now);
        assert!(
            text.contains("暂停等待中") && text.contains("在等：更早的一条 Run"),
            "{text}"
        );
        assert_eq!(snapshot(None).render(now), "#2a9c「抓数据」：还没有开始跑");
    }

    #[test]
    fn durations_switch_unit_at_a_minute_and_an_hour() {
        assert_eq!(duration_words(time::Duration::seconds(8)), "8 秒");
        assert_eq!(duration_words(time::Duration::seconds(125)), "2 分 5 秒");
        assert_eq!(
            duration_words(time::Duration::seconds(3 * 3600 + 7 * 60 + 9)),
            "3 时 7 分"
        );
        assert_eq!(duration_words(time::Duration::seconds(-3)), "0 秒");
    }
}
