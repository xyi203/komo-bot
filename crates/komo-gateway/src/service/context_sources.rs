//! 为一次 Context 装配**取数**（`docs/agent.md` §7）：这里只回答"事实从哪来"，不回答
//! "模型该看见什么"——那是 `komo-agent::context` 的事。函数都写成**自由函数、参数显式
//! 传入**，不写成 `impl GatewaySegments`：这样每个函数要用哪些存储一眼看得出来，也不用
//! 造一整个 `GatewaySegments` 就能单测。目前只有一个实现，不为此抽 trait。
//!
//! 搬到这里的（§7）：
//!
//! - [`Identity`] / [`identity_for`] / [`ambient_identity`]（冻结身份与兜底身份，§4.3）
//! - [`recall_for`] / [`recall_scopes`]（记忆召回，§9.2、§9.4）
//! - [`message_text`] / [`resolve_history`]（history 的 I/O 那一半：外置正文、
//!   `output.json`，§8.3）
//! - [`skill_catalog`]（skills 目录的读取：注册表 → `SkillCatalog`，§14）
//!
//! 留在 `segment.rs` 的是"为执行取数"的那一半：`resumed` / `call_request` /
//! `waiting_approval`（续跑位置）、`run_roots` / `ResourceMounts`（`CallEnv` 的根与
//! 挂载）、`halt_if_corrupt`（失败收场）、`without_delegate`（能力收窄）——它们改的是
//! 执行环境，不是"模型看见什么"。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use komo_agent::context::history::{
    Entry, EntryKind, ResolvedMessage, StoredOutput, latest_user_text,
};
use komo_agent::context::tasks::{TaskBoard, TaskEntry, TaskState};
use komo_agent::skills::{OfferContext, SkillCatalog, SkillRegistry};
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::fold::{Surface, SurfaceMessage};
use komo_kernel::traits::{Ledger, LedgerError, ToolOutputStore};
use komo_kernel::types::agent::RunSnapshot;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::memory::{Injection, MemoryScope};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::status::{RunEnd, RunState};
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::ToolDefinition;
use komo_runtime::config::ConfigHolder;
use komo_runtime::memory::MemoryManager;
use komo_runtime::tools::paths;
use komo_store::db::store_to_ledger;
use komo_store::models::SessionKind;
use komo_store::repos::runs::RunRecord;
use komo_store::{CheckpointStore, Db, PayloadStore};
use time::OffsetDateTime;

/// 一段执行要用到的**身份与能力**（§4.3）。
///
/// 它要么来自 `run.accepted` 里冻结的那一份，要么（旧行 / Cron / 没有快照的子 Run）由
/// 当前配置的默认 Agent 兜底算出来——见 [`identity_for`]。这里的每一格都进这一段的装配：
/// 提示、Schema、工作目录、记忆作用域。
pub(crate) struct Identity {
    /// 这一段的 Agent（进日志；判据是下面那几样）。
    pub agent_id: String,
    /// 冻结下来的模型；`None` = 兜底路径，用 `runs.model_snapshot` 那一份。
    pub model: Option<ModelConfig>,
    /// 这次允许调用的工具。**Schema 与执行器查找用的是同一个它**（§4 末）。
    pub surface: AgentSurface,
    /// 解析过的真实工作目录。
    pub workspace: PathBuf,
    /// 身份指令正文（按引用读回来的那一份）。
    pub instructions: Option<String>,
    /// 记忆作用域（§9.2）。
    pub memory_scope: Option<MemoryScope>,
    /// 分发器任务看板（`docs/home-dispatcher.md` §5、§9 Phase 3）：受理这一刻冻结的那一份，
    /// 按引用读回来。`None` = 不是分发器 Run（或者是没有冻结快照的旧行/子 Run，走
    /// [`ambient_identity`] 那条兜底路——分发器 Run 永远走 `submit`，一定有冻结快照）。
    pub tasks: Option<TaskBoard>,
}

/// 没有冻结快照时兜底身份要用到的那几样：当前配置、会话 workdir、`workspaces/`（§7）。
pub(crate) struct Fallback<'a> {
    pub config: Option<&'a Arc<ConfigHolder>>,
    pub record: Option<&'a komo_store::repos::session::SessionRecord>,
    pub workspaces: &'a Path,
}

/// 这一段的**身份与能力**（§4.3）。
///
/// 三条路，按优先级：
///
/// 1. **自己那条 `run.accepted` 里冻结的那一份**——正常的交互 Run 都走这条。审批可能
///    一小时之后才答复，那时 Profile 与磁盘都可能已经改过，恢复出来的 Run 不能因此
///    换一副面孔；
/// 2. **父 Run 的快照**——子 Run 受理时拿不到 Profile、工作目录与指令正文（执行器手里
///    只有父的 `CallEnv`），而它本来就是父那次委派的延续：同一个 Agent、同一个目录、
///    同一份指令，只是不能再委派（§4 的深度只有一层，摘名字在 `segment()` 里做）；
/// 3. **当前配置的默认 Agent**——旧行（这次改造之前受理的）与 Cron（没有归属的入口）。
///    §八「已有 Session 归入默认 Agent；旧日志不重写」。
///
/// **没有第四条路**：当前 Profile 不许覆盖一条已经受理的 Run。
pub(crate) async fn identity_for(
    events: &[Event],
    run: &RunId,
    payloads: &PayloadStore,
    fallback: Fallback<'_>,
    catalog: &[ToolDefinition],
) -> Result<Identity, LedgerError> {
    let frozen = frozen_snapshot(events, run).or_else(|| {
        delegate_parent(events, run).and_then(|parent| frozen_snapshot(events, &parent))
    });
    let Some(frozen) = frozen else {
        return Ok(ambient_identity(fallback, catalog));
    };
    // 身份指令按引用读回来（§4.3：存的是内容，不是文件路径）。读不出来 = 会话缺内容，
    // 由调用方停下来报告——拿当前配置编一份提示继续跑，正是"换了一副面孔"。
    let instructions = match &frozen.instructions_ref {
        Some(reference) => {
            let bytes = payloads.open(reference).await.map_err(store_to_ledger)?;
            Some(String::from_utf8(bytes).map_err(|error| {
                LedgerError::Corrupt(format!("冻结的身份指令不是 UTF-8：{error}"))
            })?)
        }
        None => None,
    };
    // 分发器任务看板（`docs/home-dispatcher.md` §9 Phase 3）：受理那一刻冻结的那一份，
    // 续跑原样读回——不重新查一遍任务会话表，`[home] mode` 热重载因此改不动一条已经在跑
    // 的 Run 看到的看板。
    let tasks = match &frozen.dispatcher_tasks_ref {
        Some(reference) => Some(
            payloads
                .open_json::<TaskBoard>(reference)
                .await
                .map_err(store_to_ledger)?,
        ),
        None => None,
    };
    Ok(Identity {
        agent_id: frozen.agent_id,
        model: Some(frozen.model),
        surface: frozen.surface,
        // 快照里存的是受理那一刻解析过的真实路径；再解一次是幂等的（目录后来被删掉
        // 也只会把还不存在的那几段按字面接回去）。
        workspace: paths::real_root(&frozen.workspace),
        instructions,
        memory_scope: frozen.memory_scope,
        tasks,
    })
}

/// 没有冻结快照时的兜底身份：**当前**配置里、这个会话归属的那个 Agent（`docs/
/// home-dispatcher.md` §8 Fix 2）。
///
/// 会话没有归属（空串，升级前建的行）或者归属的 Agent 已经从 `[agents]` 里删掉了，才退到
/// 默认 Agent——与 `GatewayState::owner_or_default` / `freeze_run` 同一条规则
/// （`AgentConfig::profile_of`），不是一条各算各的兜底：任务会话（`worker` Profile）走的
/// 正是这条没有冻结快照的路，答错了 Agent 就是拿错误的身份跑完整个任务。
///
/// 工作目录的取舍与受理那一步是**同一条规则**（会话 `workdir` → Profile `workspace`
/// → `workspaces/`）：同一件事在两条路上有两个答案，正是 §八 修掉的那类缺口。
fn ambient_identity(fallback: Fallback<'_>, catalog: &[ToolDefinition]) -> Identity {
    let names: Vec<String> = catalog.iter().map(|tool| tool.name.clone()).collect();
    let config = fallback.config.map(|config| config.current());
    let agent_id = fallback
        .record
        .map(|record| record.agent_id.as_str())
        .unwrap_or_default();
    let profile = config
        .as_ref()
        .map(|config| config.agent.profile_of(agent_id));
    let surface = match (profile, fallback.config) {
        (Some(profile), Some(holder)) => {
            komo_agent::surface_of(profile, &names, &holder.home().join("config.toml"))
        }
        // 精简装配（没有配置快照）：这次能用的就是目录里装着的那些。
        _ => AgentSurface::new(names),
    };
    let workspace = fallback
        .record
        .and_then(|record| record.workdir.clone())
        .map(PathBuf::from)
        .or_else(|| profile.and_then(|profile| profile.workspace.clone()))
        .unwrap_or_else(|| fallback.workspaces.to_path_buf());
    Identity {
        agent_id: profile
            .map(|profile| profile.id.clone())
            .unwrap_or_default(),
        // 模型这一格留空：调用方回落到 `runs.model_snapshot`（受理时写下的那一份）。
        model: None,
        surface,
        workspace: paths::real_root(&workspace),
        instructions: profile.and_then(|profile| profile.instructions.clone()),
        memory_scope: profile.and_then(|profile| profile.memory_scope.clone()),
        // 没有冻结快照的路从来不会是分发器 Run（分发器永远走 `submit`/`freeze_run`，
        // 一定有快照）；这条兜底路只服务旧行 / 没有归属的子 Run。
        tasks: None,
    }
}

/// 这一条 Run 受理时冻结下来的身份与能力（§4.3）。
///
/// 它落在**事件**里（`run.accepted`），所以重启之后读得到——这正是"恢复时按它装配"
/// 那句话的落点。
fn frozen_snapshot(events: &[Event], run: &RunId) -> Option<Box<RunSnapshot>> {
    events.iter().find_map(|event| match &event.payload {
        EventPayload::RunAccepted(body) if event.run.as_ref() == Some(run) => body.snapshot.clone(),
        _ => None,
    })
}

/// 派这一条 Run 的那条 Run（§4；普通 Run 是 `None`）。
fn delegate_parent(events: &[Event], run: &RunId) -> Option<RunId> {
    events.iter().find_map(|event| match &event.payload {
        EventPayload::RunAccepted(body) if event.run.as_ref() == Some(run) => {
            body.delegate.as_ref().map(|spec| spec.parent.clone())
        }
        _ => None,
    })
}

/// 这一次召回放行哪些作用域（§9.2）。
///
/// `None`（Profile 没写 `memory_scope`）= **不按作用域过滤**——`RecallQuery::scopes` 空
/// 就是"全部"，与改造之前的行为一致。写了就只召回那一个，外加 [`MemoryScope::Personal`]：
/// 用户资料是**显式共享**的那一份，每个 Agent 都该看得见（否则"用户偏好深色主题"要为每个
/// Agent 各记一遍，而它们本来就都是同一个操作者的事）。
pub(crate) fn recall_scopes(scope: Option<&MemoryScope>) -> Vec<MemoryScope> {
    let Some(scope) = scope else {
        return Vec::new();
    };
    let mut scopes = vec![scope.clone()];
    if *scope != MemoryScope::Personal {
        scopes.push(MemoryScope::Personal);
    }
    scopes
}

/// 这一段注入哪些记忆（§9.4、§9.7）。
///
/// 查询文本是回放面上**最后一条用户消息**——「当前用户输入 + 少量任务上下文」
/// （§9.4）。续跑时先把检查点里记的那一批交给 [`MemoryManager::prepare`] 重新核对：
/// 活下来的沿用（不必再问一次 embedding），已经遗忘或改了版本的掉出去（§9.7）。
///
/// `scopes` 来自 Profile（`memory_scope`）：**过滤发生在检索里**，不是拿到结果之后再
/// 遮掉别人 Agent 的记录（§五）。
pub(crate) async fn recall_for(
    memories: Option<&Arc<MemoryManager>>,
    checkpoints: Option<&CheckpointStore>,
    session: &komo_kernel::types::ids::SessionId,
    run: &RunId,
    surface: &Surface,
    scopes: &[MemoryScope],
) -> Injection {
    let Some(memories) = memories else {
        return Injection::default();
    };
    let carried = match checkpoints {
        Some(store) => match store.latest(session).await {
            Ok(Some(record)) => record.memories,
            Ok(None) => Vec::new(),
            Err(error) => {
                tracing::debug!(%error, "读不出检查点，这一段重新召回");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let text = latest_user_text(surface).unwrap_or_default();
    // **按"这一段对话"取**，不是按这一轮重新召回（§9.4）：注入段在 system 消息里，
    // 每轮重算一次就等于每轮把服务端前缀缓存打掉一次。
    memories
        .prepare_segment(session, run, &text, &carried, surface.boundary(), scopes)
        .await
}

/// 这一次装配的 skills 目录（§14）：读一次**活注册表**，按门控算出这一份值。
///
/// `None` = 没接注册表（精简装配、`skills` 关掉的部署，或者这是子代理——子代理不看目录，
/// 判据在调用方，§12）。
pub(crate) fn skill_catalog(
    skills: Option<&RwLock<SkillRegistry>>,
    tool_names: &[String],
) -> Option<SkillCatalog> {
    let registry = skills?.read().expect("skills 注册表");
    Some(registry.offer(&OfferContext::here(tool_names.iter().cloned())))
}

/// 任务看板"最近完成"那一半的条数上限（`docs/home-dispatcher.md` §5、§11：默认值，
/// 目前不可配置）。
pub(crate) const TASK_BOARD_RECENT_LIMIT: usize = 10;

/// 只看这个窗口内结束的任务（`docs/home-dispatcher.md` §11：默认 24 小时）。
fn task_board_recent_window() -> time::Duration {
    time::Duration::hours(24)
}

/// 候选池上限：按创建时间只看最近这么多个**已结束**的任务会话，再从中挑
/// [`TASK_BOARD_RECENT_LIMIT`] 个落在窗口内的——避免一个用了很久的 home 为每一次分发器
/// Run 都读遍它全部历史任务会话的 `runs` 表（§5 的"避免 N+1"）。
const TASK_BOARD_CANDIDATE_POOL: usize = TASK_BOARD_RECENT_LIMIT * 5;

/// 分发器的任务看板取数（`docs/home-dispatcher.md` §5）：这个 home 名下的任务会话
/// （`kind = task`、`origin = task:{home}`），进行中的全部 + 最近完成的一批。
///
/// **状态与等待原因从 `runs` 表读**（索引化的单行/按会话过滤读取），不重放整段 JSONL；
/// 只有"最后一条回复的首行"要读事件正文，而这条路走的是 [`Ledger::run_end`]——它按
/// `final_event` 定位那一条事件附近的一页并把外置的大正文按引用读回来（§8.3），不是
/// `http::sessions::summary_of` 那种"整段 JSONL fold 一遍"的读法（那正是这里要避免的
/// N+1）。
pub(crate) async fn task_board(
    db: &Db,
    ledger: &dyn Ledger,
    home_session: &SessionId,
    now: OffsetDateTime,
) -> Result<TaskBoard, LedgerError> {
    let origin = super::tasks::task_origin(home_session);
    let records = komo_store::repos::session::list(db, false)
        .await
        .map_err(store_to_ledger)?;

    let mut unfinished = Vec::new();
    let mut finished = Vec::new();
    for record in records {
        if record.kind != SessionKind::Task || record.origin != origin {
            continue;
        }
        if record.current_run.is_some() {
            unfinished.push(record);
        } else {
            finished.push(record);
        }
    }

    let mut entries = Vec::with_capacity(unfinished.len());
    for record in unfinished {
        if let Some((entry, _)) = task_entry_of(db, ledger, &record).await? {
            entries.push(entry);
        }
    }

    // 最近**创建**的排在前面，只读这一批的 `runs` 表——真正的"最近完成"排序在下面按
    // `ended_at` 做。
    finished.sort_by(|a, b| b.session.as_str().cmp(a.session.as_str()));
    finished.truncate(TASK_BOARD_CANDIDATE_POOL);

    let window = task_board_recent_window();
    let mut recent: Vec<(OffsetDateTime, TaskEntry)> = Vec::new();
    for record in finished {
        let Some((entry, run)) = task_entry_of(db, ledger, &record).await? else {
            continue;
        };
        let Some(ended_at) = run.ended_at else {
            continue;
        };
        if now - ended_at > window {
            continue;
        }
        recent.push((ended_at, entry));
    }
    recent.sort_by(|a, b| b.0.cmp(&a.0));
    entries.extend(
        recent
            .into_iter()
            .take(TASK_BOARD_RECENT_LIMIT)
            .map(|(_, entry)| entry),
    );

    Ok(TaskBoard { entries })
}

/// 一个任务会话渲染成看板条目：取它**最新**那条 Run（`current_run` 有值就是它，否则是
/// `runs::list_for_session` 排出来的最后一条——同一 Session 严格串行，最新的就是最后
/// 一条），连同那条 Run 一起返回（调用方要读它的 `ended_at` 做窗口过滤）。
async fn task_entry_of(
    db: &Db,
    ledger: &dyn Ledger,
    record: &komo_store::repos::session::SessionRecord,
) -> Result<Option<(TaskEntry, RunRecord)>, LedgerError> {
    let run = match &record.current_run {
        Some(run_id) => komo_store::repos::runs::get(db, &RunId::from_raw(run_id.clone()))
            .await
            .map_err(store_to_ledger)?,
        None => komo_store::repos::runs::list_for_session(db, &record.session)
            .await
            .map_err(store_to_ledger)?
            .pop(),
    };
    let Some(run) = run else {
        return Ok(None);
    };
    let state = task_state_of(&run);
    // 只有"正常跑完"才有一句"最后回复"：失败 / 取消 / 放弃已经由状态本身说清楚了，
    // 编一句"回复"反而是编造。
    let last_reply = if run.state == RunState::Completed {
        match ledger.run_end(&run.run).await? {
            Some(RunEnd::Completed { final_message, .. }) => final_message,
            _ => None,
        }
    } else {
        None
    };
    let entry = TaskEntry {
        session: record.session.clone(),
        title: record.title.clone(),
        state,
        last_reply,
    };
    Ok(Some((entry, run)))
}

/// `RunRecord` 的调度状态 → 看板状态（`docs/home-dispatcher.md` §5）。
fn task_state_of(run: &RunRecord) -> TaskState {
    match run.state {
        RunState::Accepted | RunState::Queued => TaskState::Queued,
        RunState::Running => TaskState::Running,
        RunState::Waiting => TaskState::Waiting(run.wait.clone()),
        RunState::Completed => TaskState::Completed,
        RunState::Failed => TaskState::Failed,
        RunState::Cancelled => TaskState::Cancelled,
        RunState::Abandoned => TaskState::Abandoned,
    }
}

/// 一条消息的正文：内联的那份优先，没有就按引用读回来（§8.3）。
///
/// 只读内联那份会让超限的正文在回放里变成**空消息**：用户输入过 4 KiB 时，模型看到的是
/// 一个空的 user，而它该看到的是原话。外置正文的哈希由 `PayloadStore::open` 校验，对不上
/// 就是会话缺内容，不返回内容。
pub(crate) async fn message_text(
    payloads: &PayloadStore,
    message: &SurfaceMessage,
) -> Result<Option<String>, LedgerError> {
    if let Some(text) = &message.text {
        return Ok(Some(text.clone()));
    }
    let Some(reference) = &message.text_ref else {
        return Ok(None);
    };
    let bytes = payloads.open(reference).await.map_err(store_to_ledger)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| LedgerError::Corrupt(format!("外置正文不是 UTF-8：{error}")))
}

/// history 的 I/O 那一半：只对 `entries` 选中的条目读正文与 `output.json`（§7、§8）。
///
/// **只有 `Protocol` 条目才会去读工具输出**：`Transcript` 条目不带工具往返，`outputs`
/// 恒为空。读不回来的正文（外置引用坏了）让整段装配失败——调用方按 §8.4 停下来报告，
/// 不把一条空消息当成"用户就是这么说的"发出去；`output.json` 读不回来则不是错误，只是
/// 退回账本里那份预览，交给 [`komo_agent::context::history::to_replay_messages`] 处理。
pub(crate) async fn resolve_history<'s>(
    entries: Vec<Entry<'s>>,
    payloads: &PayloadStore,
    outputs: Option<&dyn ToolOutputStore>,
) -> Result<Vec<ResolvedMessage<'s>>, LedgerError> {
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = message_text(payloads, entry.message).await?;
        let mut stored = Vec::new();
        if matches!(entry.kind, EntryKind::Protocol) {
            for result in &entry.message.tool_results {
                let verified = match outputs {
                    Some(outputs) => outputs.open(&result.output).await.ok(),
                    None => None,
                };
                stored.push(verified.map(|verified| StoredOutput {
                    preview: verified.body.preview.clone(),
                    artifacts: verified.body.artifacts.clone(),
                }));
            }
        }
        resolved.push(ResolvedMessage {
            entry,
            text,
            outputs: stored,
        });
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    //! 真的要读 `PayloadStore` 的那几条测试：`entries` 的选窗口本身是纯的（测试在
    //! `komo-agent`），这里测的是 `resolve_history` 的 I/O 那一半——外置正文真的读得回来、
    //! 读不回来就不装作读到了。走的是生产路径：`entries` → `resolve_history` →
    //! `komo_agent::context::assemble`。

    use super::*;
    use komo_agent::context::history::{ReplayScope, entries};
    use komo_agent::context::{ContextInput, InvocationContext, assemble};
    use komo_kernel::events::{Event, EventPayload, RunAccepted, RunStarted};
    use komo_kernel::fold::fold;
    use komo_kernel::types::digest::ContentHash;
    use komo_kernel::types::ids::{EventId, ExecutorId, RequestKey, RunId, Seq, SessionId};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::refs::PayloadRef;

    fn payloads() -> PayloadStore {
        let dir = tempfile::tempdir().expect("临时目录");
        PayloadStore::new(komo_store::SessionPaths::at(dir.keep()))
    }

    fn event(seq: u64, run: &RunId, payload: EventPayload) -> Event {
        Event {
            v: 1,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw("sess-1"),
            run: Some(run.clone()),
            ts: time::macros::datetime!(2026-09-16 08:00:00 UTC),
            payload,
        }
    }

    fn accepted_as(
        run: &RunId,
        seq: u64,
        text: Option<&str>,
        text_ref: Option<PayloadRef>,
    ) -> Event {
        event(
            seq,
            run,
            EventPayload::RunAccepted(RunAccepted {
                request_key: RequestKey::new(format!("key-{seq}")),
                input_hash: ContentHash::of_str(text.unwrap_or_default()),
                text: text.map(str::to_string),
                text_ref,
                source: PlanSource::Interactive {
                    session: SessionId::from_raw("sess-1"),
                },
                peer: None,
                model: None,
                effort: None,
                delegate: None,
                snapshot: None,
            }),
        )
    }

    fn started(run: &RunId, seq: u64) -> Event {
        event(
            seq,
            run,
            EventPayload::RunStarted(RunStarted {
                executor: ExecutorId::from_raw("exec-1"),
                generation: 1,
            }),
        )
    }

    async fn context_of(
        events: &[Event],
        run: &RunId,
        payloads: &PayloadStore,
    ) -> Result<komo_agent::context::AgentContext, LedgerError> {
        let surface = fold(events);
        let selected = entries(&surface, ReplayScope::Conversation(run));
        let resolved = resolve_history(selected, payloads, None).await?;
        Ok(assemble(ContextInput {
            instructions: None,
            workspace: PathBuf::from("/work"),
            tools: Vec::new(),
            history: resolved,
            memory: None,
            skills: None,
            tasks: None,
            invocation: InvocationContext::Main,
            komo_exe: None,
            model_result_bytes: 8 * 1024,
        }))
    }

    /// 超限的正文外置在 `payloads/` 里：回放要按引用读回来（§8.3）。
    ///
    /// 只读内联那份的话，用户输入过 4 KiB 时模型看到的是一个**空的 user 消息**。
    #[tokio::test]
    async fn an_externalized_message_comes_back_by_reference() {
        let payloads = payloads();
        let big = "把这段话原样带回来。".repeat(600);
        assert!(big.len() > 4 * 1024, "要真的过内联上限");
        let question = payloads.put(big.as_bytes()).await.expect("外置得进去");
        let reply = payloads
            .put(big.as_bytes())
            .await
            .expect("同一段正文复用一份");

        let first = RunId::from_raw("run-1");
        let second = RunId::from_raw("run-2");
        let events = vec![
            accepted_as(&first, 1, None, Some(question)),
            started(&first, 2),
            // 一条没有正文的轮次：它不该抢走"最后答了什么"的位置。
            event(
                3,
                &first,
                EventPayload::MessageAssistant(komo_kernel::events::MessageAssistant {
                    round: 1,
                    text: None,
                    text_ref: None,
                    tool_calls: vec![],
                    provider_blocks: None,
                    input_tokens: None,
                    output_tokens: None,
                }),
            ),
            event(
                4,
                &first,
                EventPayload::MessageAssistant(komo_kernel::events::MessageAssistant {
                    round: 2,
                    text: None,
                    text_ref: Some(reply),
                    tool_calls: vec![],
                    provider_blocks: None,
                    input_tokens: None,
                    output_tokens: None,
                }),
            ),
            accepted_as(&second, 5, Some("接着上面那句"), None),
            started(&second, 6),
        ];

        let context = context_of(&events, &second, &payloads)
            .await
            .expect("读得出来");
        assert_eq!(
            context.messages[0].text.as_deref(),
            Some(big.as_str()),
            "外置的用户输入要按引用读回来"
        );
        assert_eq!(
            context.messages[1].text.as_deref(),
            Some(big.as_str()),
            "外置的模型回复同理（最后一条带正文的那条）"
        );
    }

    /// 引用读不出来 = 会话缺内容：停下来报告，而不是把一条空消息发出去（§8.3）。
    #[tokio::test]
    async fn a_payload_that_cannot_be_read_stops_the_segment() {
        let missing = payloads()
            .put("这份正文不在那个目录里".as_bytes())
            .await
            .expect("外置得进去");
        let run = RunId::from_raw("run-1");
        let events = vec![accepted_as(&run, 1, None, Some(missing)), started(&run, 2)];

        // 故意用一个**不同的** `PayloadStore`：那份引用不在这里，读不出来。
        let error = context_of(&events, &run, &payloads())
            .await
            .expect_err("读不出来就不该装作读到了");
        assert!(matches!(error, LedgerError::Corrupt(_)), "{error:?}");
    }

    /// Fix 2（`docs/home-dispatcher.md` §8）：没有冻结快照时，兜底身份要看会话自己的
    /// `agent_id`——配的是哪个 Profile 就用哪个的指令与能力面，不能总是退到默认 Agent。
    /// 任务会话（`worker` Profile）走的正是这条没有冻结快照的路，答错了 Agent 就是拿
    /// 错误的身份跑完整个任务。
    #[tokio::test]
    async fn ambient_identity_uses_the_sessions_agent_when_it_names_a_configured_profile() {
        use std::collections::BTreeMap;

        use komo_kernel::types::agent::{AgentConfig, AgentProfile};
        use komo_kernel::types::status::SessionState;
        use komo_runtime::config::{EffortCapabilities, Loaded, Secrets, Sources};
        use komo_store::models::SessionKind;
        use komo_store::repos::session::SessionRecord;

        let payloads = payloads();
        let run = RunId::from_raw("run-1");
        // `accepted_as` 定死不带快照：旧行 / 没有冻结身份的子 Run 走的都是这条路。
        let events = vec![
            accepted_as(&run, 1, Some("查一下 3 号房的空调"), None),
            started(&run, 2),
        ];

        let mut snapshot = komo_kernel::test_support::snapshot_fixture();
        snapshot.agent = AgentConfig {
            default_agent: "assistant".into(),
            agents: BTreeMap::from([
                ("assistant".into(), AgentProfile::new("assistant")),
                (
                    "coder".into(),
                    AgentProfile {
                        instructions: Some("你是 coder，只管写代码。".into()),
                        tools: Some(vec!["read".into()]),
                        ..AgentProfile::new("coder")
                    },
                ),
            ]),
        };
        let home = PathBuf::from("/tmp/komo-ambient-identity-test");
        let holder = Arc::new(ConfigHolder::adopt(
            Loaded {
                snapshot: Arc::new(snapshot),
                secrets: Arc::new(Secrets::new()),
                issues: Vec::new(),
                home: home.clone(),
                sources: Sources::under(&home),
            },
            EffortCapabilities::builtin(),
        ));

        let session_record = SessionRecord {
            session: SessionId::from_raw("sess-1"),
            title: String::new(),
            origin: "chat:telegram:1".into(),
            agent_id: "coder".into(),
            kind: SessionKind::Main,
            workdir: None,
            current_run: None,
            jsonl_path: String::new(),
            applied_seq: Seq::ZERO,
            applied_bytes: 0,
            state: SessionState::Active,
            state_changed_at: time::macros::datetime!(2026-09-24 08:00:00 UTC),
        };

        let catalog = vec![
            ToolDefinition {
                name: "read".into(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            },
            ToolDefinition {
                name: "shell".into(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            },
        ];
        let workspaces = PathBuf::from("/workspaces");

        let identity = identity_for(
            &events,
            &run,
            &payloads,
            Fallback {
                config: Some(&holder),
                record: Some(&session_record),
                workspaces: &workspaces,
            },
            &catalog,
        )
        .await
        .expect("没有冻结快照时兜底也读得出来");

        assert_eq!(
            identity.agent_id, "coder",
            "兜底身份要按会话的归属挑 Profile"
        );
        assert_eq!(
            identity.instructions.as_deref(),
            Some("你是 coder，只管写代码。")
        );
        assert_eq!(
            identity.surface.names(),
            ["read"],
            "coder 的能力面只留它自己写的那个工具"
        );
    }
}
