//! 分发器的任务看板（`docs/home-dispatcher.md` §5）：home 分发器在每一轮之前，需要知道
//! "现在有哪些任务、它们在做什么、最后说了什么"，否则一句"1+1 等于几"和一句"再看看空调"
//! 就没法区分该不该 `follow`。
//!
//! 取数在 Gateway（`context_sources::task_board`，I/O），这里只是**presentation**：给一份
//! 已经查好的条目列表，渲染成系统提示里的一段。短号计算复用 `komo-kernel` 的
//! [`komo_kernel::types::task`]（Phase 2 已经在用的那一份，看板与 `follow` 不能有两种
//! 短号口径）。

use komo_kernel::types::ids::SessionId;
use komo_kernel::types::status::WaitReason;
use serde::{Deserialize, Serialize};

use komo_kernel::types::task;

/// 一次装配看到的任务看板：这个 home 名下、进行中的全部 + 最近完成的一批
/// （`docs/home-dispatcher.md` §5、§11：默认 10 条 / 24 小时）。
///
/// 冻结的正是这份值（`docs/home-dispatcher.md` §9 Phase 3）：受理这一刻序列化进 payload，
/// 续跑原样读回，所以要能整份 round-trip。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBoard {
    pub entries: Vec<TaskEntry>,
}

/// 看板上的一条：一个任务会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEntry {
    /// 任务会话本身——短号从它现算（渲染时才知道要不要延长到 6 位，§4.3）。
    pub session: SessionId,
    pub title: String,
    pub state: TaskState,
    /// 这个任务**最后一次跑完**（`Completed`）时的完整回复；渲染时只取首行、截到
    /// [`REPLY_PREVIEW_CHARS`]。`None` = 还没有一次跑完过，或者最近这一次不是正常结束
    /// （失败 / 取消 / 放弃——状态本身已经说清楚了，不需要再编一句"回复"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reply: Option<String>,
}

/// 一个任务此刻的状态（`docs/home-dispatcher.md` §5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    /// 排队中：有活干，只是还没轮到（§8.4）。
    Queued,
    Running,
    /// 停在某个外部条件上。`None` = 停着但账本里没记原因（不该发生，但渲染不因此崩）。
    Waiting(Option<WaitReason>),
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

/// 最后一条回复只取首行，且截到这个长度（字符数）——看板是一句话提示，不是转述。
const REPLY_PREVIEW_CHARS: usize = 80;

/// 空看板时的一行提示（`docs/home-dispatcher.md` §5 的决定：空看板也渲染一段，不是
/// 整段消失——分发器由此知道"现在没有任何任务在跑"，不必去猜工具没接上还是真的没有）。
const EMPTY_BOARD_LINE: &str = "（还没有任务）";

/// 渲染成系统提示里的一段（`docs/agent.md` §10：skills 之后、memory 之前）。
///
/// 纯函数：同一份 [`TaskBoard`] 永远渲染出同一段文本——冻结的意义正在于此（§9 Phase 3）。
pub fn prompt_block(board: &TaskBoard) -> String {
    let mut lines = vec!["## 任务看板".to_string()];
    if board.entries.is_empty() {
        lines.push(EMPTY_BOARD_LINE.to_string());
        return lines.join("\n");
    }
    // 撞号才延长：同一个 home 下进行中与最近 N 个任务里，4 位短号可能撞上（§4.3）。
    // 这里按"整个看板"决定要不要延长，而不是只延长撞上的那几条——同一段提示里所有条目
    // 用同一个位数，模型与操作者对话时报出来的号才不会一会 4 位一会 6 位。
    let extend = has_collision(&board.entries);
    for entry in &board.entries {
        let short = if extend {
            task::extended_short_id(&entry.session)
        } else {
            task::short_id(&entry.session)
        };
        let mut line = format!("- #{short} {}：{}", entry.title, state_text(&entry.state));
        if let Some(reply) = entry.last_reply.as_deref() {
            let preview = first_line(reply, REPLY_PREVIEW_CHARS);
            if !preview.is_empty() {
                line.push_str("——");
                line.push_str(&preview);
            }
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// 4 位短号在这份看板里有没有撞。
fn has_collision(entries: &[TaskEntry]) -> bool {
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .any(|entry| !seen.insert(task::short_id(&entry.session)))
}

/// 一句状态文案。等待原因按种类给一句话，不编具体的"还要多久"——看板冻结之后不再更新，
/// 写一个当时算出来的倒计时只会立刻过期（§9 Phase 3）。
fn state_text(state: &TaskState) -> &'static str {
    match state {
        TaskState::Queued => "排队中",
        TaskState::Running => "运行中",
        TaskState::Waiting(reason) => wait_text(reason.as_ref()),
        TaskState::Completed => "已完成",
        TaskState::Failed => "失败",
        TaskState::Cancelled => "已取消",
        TaskState::Abandoned => "已放弃",
    }
}

fn wait_text(reason: Option<&WaitReason>) -> &'static str {
    match reason {
        Some(WaitReason::Approval { .. }) => "等待审批",
        Some(WaitReason::Intervention { .. }) => "需要你判断",
        Some(WaitReason::Retry { .. }) => "稍后重试",
        Some(WaitReason::Dependency { .. }) => "排在前一个任务后面",
        None => "等待中",
    }
}

/// 首行，按**字符数**截断到 `max`（中文一个字算一个），超出加 `…`。
fn first_line(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(max).collect();
    if first.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::{ApprovalId, RunId};

    fn entry(session: &str, title: &str, state: TaskState, reply: Option<&str>) -> TaskEntry {
        TaskEntry {
            session: SessionId::from_raw(session),
            title: title.to_string(),
            state,
            last_reply: reply.map(str::to_string),
        }
    }

    /// 空看板不是"没有这一段"——分发器需要**知道**现在没有任务，而不是猜。
    #[test]
    fn an_empty_board_still_renders_a_line() {
        let block = prompt_block(&TaskBoard { entries: vec![] });
        assert_eq!(block, "## 任务看板\n（还没有任务）");
    }

    /// 每条：短号、标题、状态；等待原因按种类给一句话。
    #[test]
    fn a_waiting_entry_names_its_short_id_and_reason() {
        let board = TaskBoard {
            entries: vec![entry(
                "0190f000-aaaa-7000-8000-0000003f2a9c",
                "查空调状态",
                TaskState::Waiting(Some(WaitReason::Approval {
                    approval: ApprovalId::from_raw("ap-1"),
                })),
                None,
            )],
        };
        let block = prompt_block(&board);
        assert!(block.contains("#2a9c"), "{block}");
        assert!(block.contains("查空调状态"), "{block}");
        assert!(block.contains("等待审批"), "{block}");
    }

    /// 最后一条回复只取**首行**，且截到 80 字符，超出加省略号。
    #[test]
    fn a_completed_entry_previews_only_the_first_line_truncated() {
        let long = "第".repeat(100);
        let reply = format!("{long}\n后面还有第二行不该出现");
        let board = TaskBoard {
            entries: vec![entry(
                "0190f000-aaaa-7000-8000-0000003f2a9c",
                "清理日志",
                TaskState::Completed,
                Some(&reply),
            )],
        };
        let block = prompt_block(&board);
        assert!(!block.contains("第二行"), "{block}");
        let expected_preview: String = long.chars().take(80).collect();
        assert!(block.contains(&format!("{expected_preview}…")), "{block}");
    }

    /// 短的回复不需要省略号。
    #[test]
    fn a_short_reply_is_not_truncated() {
        let board = TaskBoard {
            entries: vec![entry(
                "0190f000-aaaa-7000-8000-0000003f2a9c",
                "清理日志",
                TaskState::Completed,
                Some("已清理 120 个文件"),
            )],
        };
        let block = prompt_block(&board);
        assert!(block.contains("已清理 120 个文件"), "{block}");
        assert!(!block.contains('…'), "{block}");
    }

    /// 4 位短号在这份看板里撞了：**整份**看板延长到 6 位，不是只延长撞上的那两条。
    #[test]
    fn colliding_short_ids_extend_the_whole_board_to_six_characters() {
        let first_id = "0190f000-aaaa-7000-8000-000000cc2a9c";
        let second_id = "0190f000-bbbb-7000-8000-000000dd2a9c";
        let board = TaskBoard {
            entries: vec![
                entry(first_id, "任务甲", TaskState::Running, None),
                entry(second_id, "任务乙", TaskState::Running, None),
            ],
        };
        let block = prompt_block(&board);
        // 两条的末 4 位都是 `2a9c`：延长到 6 位之后应该各自不同（`cc2a9c` / `dd2a9c`）。
        let first = task::extended_short_id(&SessionId::from_raw(first_id));
        let second = task::extended_short_id(&SessionId::from_raw(second_id));
        assert_ne!(first, second);
        assert!(block.contains(&format!("#{first}")), "{block}");
        assert!(block.contains(&format!("#{second}")), "{block}");
        assert!(!block.contains("#2a9c"), "{block}");
    }

    /// 没撞号时保持 4 位——不必要地拉长看板里的每一行。
    #[test]
    fn distinct_short_ids_stay_at_four_characters() {
        let board = TaskBoard {
            entries: vec![
                entry(
                    "0190f000-aaaa-7000-8000-0000003f2a9c",
                    "任务甲",
                    TaskState::Running,
                    None,
                ),
                entry(
                    "0190f000-bbbb-7000-8000-000000aa11bb",
                    "任务乙",
                    TaskState::Running,
                    None,
                ),
            ],
        };
        let block = prompt_block(&board);
        assert!(block.contains("#2a9c"), "{block}");
        assert!(block.contains("#11bb"), "{block}");
    }

    /// 冻结要能整份 round-trip（§9 Phase 3）。
    #[test]
    fn a_board_round_trips_through_json() {
        let board = TaskBoard {
            entries: vec![entry(
                "0190f000-aaaa-7000-8000-0000003f2a9c",
                "查空调状态",
                TaskState::Waiting(Some(WaitReason::Dependency {
                    run: RunId::from_raw("run-1"),
                })),
                Some("已经查过了"),
            )],
        };
        let json = serde_json::to_vec(&board).expect("序列化");
        let back: TaskBoard = serde_json::from_slice(&json).expect("反序列化");
        assert_eq!(back, board);
    }
}
