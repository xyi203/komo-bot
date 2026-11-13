//! `komo`：操作 komo 自己的状态——记忆、目标、定时任务、找会话、任务清单（`docs/tasks.md`
//! §4.4）。不经 shell，不走 HTTP 回环：动作在 Gateway 进程里执行。
//!
//! 它不是"做事"的工具（`docs/komo_bot.md` §4 的六个基础工具），是"自己的状态"这一层：
//! 以前由系统提示教模型在 `bash` 里跑 `komo <子命令>`，而命令执行的地方不一定有 `komo` 二进制、
//! 也不一定连得到 Gateway。
//!
//! # 形状
//!
//! 参数只有 `action`（"领域.动作"，enum）与 `params`（对象）。各动作的字段不进 schema，写在
//! `tool://komo/doc`（由 [`SPECS`] 现算，字段表只有这一处）；`params` 填错时 `prepare` 的报错
//! 列出该动作认的字段。
//!
//! # 接缝
//!
//! `komo-tools` 不能依赖 gateway，也看不到存储。每个动作在 `prepare` 里把 `params` 反序列化成
//! kernel 里现成的请求类型（`MemoryAddRequest`、`MatterActionRequest` …），连同这次调用的身份
//! 装成 [`KomoCall`]；`execute` 把它交给 [`KomoService`]——Gateway 的实现调 HTTP 路由背后的
//! 同一个处理函数，业务规则（例如"改承诺要有这个 Run 里的用户原话、任务会话与 Cron 被拒"）
//! 因此只有一份。接缝的空槽同 [`super::detach::DetachSlot`]：服务要等整份 `GatewayState`
//! 才造得出来，工具先拿着空槽。
//!
//! # 身份
//!
//! 会话与 Run 取自 [`ToolContext`]，不读 `KOMO_SESSION` / `KOMO_RUN`，模型也不能在 `params`
//! 里给 `session` / `run` / `request_key`（不在字段表里，被当成未知字段拒绝）。Run 取输入 Run
//! （与 `bash` 子进程的 `KOMO_RUN` 同一个口径）。请求键取这次工具调用的 id：同一次调用重试
//! 命中幂等，不同调用互不相干。
//!
//! # 重放与并发
//!
//! 只读动作（`*.search`、`*.list`）的计划是 `SafeReread`，写动作是 `NoSafeRecovery`；
//! [`Tool::replay`] 声明 `Safe`——它只能否决计划里的 `Safe`，不会把写动作变成可重跑。并发分组
//! 在 `prepare` 之前、只看名字与原始参数，所以 [`Tool::parallel_for`] 按 `action` 判断：
//! 只读动作可以与相邻的只读调用同时在飞，写动作留在顺序组里。

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use komo_kernel::protocol::http::{
    InterventionVerdict, ManualCronRunRequest, MatterActionRequest, MatterAddRequest,
    MatterListQuery, MemoryAddRequest, MemoryListQuery, MemoryRevisionRequest, SessionListQuery,
    SessionSearchQuery, TaskListQuery,
};
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::{
    CronJobId, MatterId, MemoryId, OperationId, RequestKey, RunId, SessionId,
};
use komo_kernel::types::matter::{MatterKind, MatterTrigger, MatterVerb};
use komo_kernel::types::memory::{MEMORY_QUERY_MARKER, MemoryScope};
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode, Replay};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::{parse_args, plan_time};

/// 工具名。
pub const KOMO_TOOL: &str = "komo";

/// 一个动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    MemoryAdd,
    MemorySearch,
    MemoryForget,
    GoalAdd,
    GoalNext,
    GoalWait,
    GoalResume,
    GoalDone,
    GoalLink,
    GoalList,
    GoalAttach,
    GoalDetach,
    CronList,
    CronRun,
    SessionSearch,
    SessionList,
    TaskList,
    InterventionList,
    InterventionAnswer,
}

/// 一个字段：名字、是否必填、一句话说明。
#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    required: bool,
    note: &'static str,
}

const fn field(name: &'static str, required: bool, note: &'static str) -> Field {
    Field {
        name,
        required,
        note,
    }
}

/// 一个动作的说明：名字、读写、一句话、字段表。`tool://komo/doc` 与 `params` 填错时的报错
/// 都从这里来。
struct Spec {
    action: Action,
    name: &'static str,
    read_only: bool,
    summary: &'static str,
    fields: &'static [Field],
}

const REVISION: Field = field(
    "revision",
    false,
    "预期版本（上一次返回里的 revision）；不符报版本冲突，不给就按当前版本改",
);

const PAGING: [Field; 3] = [
    field("all", false, "true = 带上已归档的"),
    field("limit", false, "一页最多几条（1–500）；不给就是全部"),
    field(
        "cursor",
        false,
        "上一页返回的 next_cursor，只在给了 limit 时有意义",
    ),
];

const TASK_LIST_FIELDS: [Field; 4] = [
    PAGING[0],
    PAGING[1],
    PAGING[2],
    field(
        "goal",
        false,
        "只要带着这个目标的键的任务（目标 id；目标已结案或已不存在也能按它找）",
    ),
];

const SPECS: [Spec; 19] = [
    Spec {
        action: Action::MemoryAdd,
        name: "memory.add",
        read_only: false,
        summary: "记一条用户要你记住的话。记成用户原话、未确认；用户说了才记，别替用户 confirm",
        fields: &[
            field("content", true, "一句简短、只说一件事的陈述"),
            field("kind", false, "preference（默认）/ fact / experience"),
            field(
                "scope",
                false,
                "personal（默认）/ project:<id> / environment:<id>",
            ),
            field(
                "application",
                false,
                "convention（长期约定）/ reference（线索，默认）；convention 只给 preference，且必须带 applies_when",
            ),
            field(
                "applies_when",
                false,
                "约定适用的条件，一句能独立理解的话，例如「创建发布工单、需要选 Platform 时」",
            ),
            field(
                "belongs_to",
                false,
                "为哪件事记的：目标的 id；必须存在且未结案",
            ),
        ],
    },
    Spec {
        action: Action::MemorySearch,
        name: "memory.search",
        read_only: true,
        summary: "按关键词查记忆（只读）。注入的记忆不够用、或报错后想看以前的排查经验时用",
        fields: &[
            field("query", true, "关键词"),
            field(
                "scope",
                false,
                "只查这个作用域：personal / project:<id> / environment:<id>",
            ),
            field(
                "state",
                false,
                "只看这个状态：candidate / active / contested / superseded / forgotten / archived；明确写出 contested 才查得到",
            ),
            field("mode", false, "keyword / vector / hybrid"),
            field("limit", false, "最多几条"),
            field("include_archived", false, "true = 带上随目标结案归档的条目"),
        ],
    },
    Spec {
        action: Action::MemoryForget,
        name: "memory.forget",
        read_only: false,
        summary: "停用一条记忆的指定版本并失效索引（不删原文）",
        fields: &[
            field("id", true, "记忆 id"),
            field(
                "revision",
                true,
                "预期版本（memory.search 返回的 revision）",
            ),
        ],
    },
    Spec {
        action: Action::GoalAdd,
        name: "goal.add",
        read_only: false,
        summary: "新建一个目标（进行中）。要这个 Run 里有用户说的话作依据；任务会话与定时任务会话不能新建",
        fields: &[
            field("title", true, "一句能独立看懂的话，≤ 80 字"),
            field("next", false, "下一步"),
            field("due", false, "截止日期，YYYY-MM-DD"),
        ],
    },
    Spec {
        action: Action::GoalNext,
        name: "goal.next",
        read_only: false,
        summary: "改目标的下一步（进行中 / 等待中的目标）",
        fields: &[
            field("id", true, "目标 id"),
            field("next", true, "下一步，一句话；给空串清空"),
            REVISION,
        ],
    },
    Spec {
        action: Action::GoalWait,
        name: "goal.wait",
        read_only: false,
        summary: "目标转等待，写明在等谁、等什么（进行中 / 等待中的目标）",
        fields: &[
            field("id", true, "目标 id"),
            field("waiting_for", true, "在等谁、等什么"),
            REVISION,
        ],
    },
    Spec {
        action: Action::GoalResume,
        name: "goal.resume",
        read_only: false,
        summary: "目标回到进行中（等待中 / 搁置的目标），例如挂着的 monitor 交回了结果",
        fields: &[
            field("id", true, "目标 id"),
            field("next", false, "顺带改下一步"),
            REVISION,
        ],
    },
    Spec {
        action: Action::GoalDone,
        name: "goal.done",
        read_only: false,
        summary: "目标完成。要这个 Run 里有用户说的话作依据（用户说办完了）；任务会话与定时任务会话不行",
        fields: &[field("id", true, "目标 id"), REVISION],
    },
    Spec {
        action: Action::GoalLink,
        name: "goal.link",
        read_only: false,
        summary: "给目标挂一个触发器（Cron Job 或 monitor），二选一；已挂着的无变化",
        fields: &[
            field("id", true, "目标 id"),
            field("cron", false, "Cron Job id（cron.list 看得到）"),
            field(
                "monitor",
                false,
                "monitor：挂它的会话 id 与 monitor 短号，写成 SESSION_ID:MONITOR_ID（# 可省）",
            ),
            REVISION,
        ],
    },
    Spec {
        action: Action::GoalList,
        name: "goal.list",
        read_only: true,
        summary: "列出目标（只读），带状态、下一步、截止，以及每个目标被几个任务带着（tasks，不含已归档的任务）；默认只列未结案的",
        fields: &[field("all", false, "true = 带上已结案的")],
    },
    Spec {
        action: Action::GoalAttach,
        name: "goal.attach",
        read_only: false,
        summary: "把**当前任务**挂到一个目标上，表示这件事是为这个目标做的（只用于找回与显示）。\
                  只有任务会话里能用；不改目标本身，不需要用户原话。重复挂无变化。\
                  只有这件事明显是在推进某个已有目标时才挂：不要为了挂而挂，也不要为此新建目标",
        fields: &[field("id", true, "目标 id（必须存在，且是目标而不是领域）")],
    },
    Spec {
        action: Action::GoalDetach,
        name: "goal.detach",
        read_only: false,
        summary: "取消当前任务与一个目标的关联；没挂过也算成功。只有任务会话里能用",
        fields: &[field("id", true, "目标 id")],
    },
    Spec {
        action: Action::CronList,
        name: "cron.list",
        read_only: true,
        summary: "列出定时任务，带下一次与最近一次触发（只读）",
        fields: &[],
    },
    Spec {
        action: Action::CronRun,
        name: "cron.run",
        read_only: false,
        summary: "立即手动触发一个定时任务（不冒充定时触发，不写触发记录）",
        fields: &[field("id", true, "Cron Job id")],
    },
    Spec {
        action: Action::SessionSearch,
        name: "session.search",
        read_only: true,
        summary: "跨会话找回（只读）：在会话标题与正文里按关键词找，返回会话与命中片段。别 export 整个会话去 grep",
        fields: &[
            field("q", true, "关键词；空白分开的几个词要在同一条消息里都出现"),
            field("limit", false, "最多几个会话，默认 10，上限 50"),
        ],
    },
    Spec {
        action: Action::SessionList,
        name: "session.list",
        read_only: true,
        summary: "列出会话（只读），最新的在前",
        fields: &PAGING,
    },
    Spec {
        action: Action::TaskList,
        name: "task.list",
        read_only: true,
        summary: "任务清单（只读）：把会话读成面向人的任务行，带状态（进行中 / 等你 / 已交回 / 空闲 / 已归档）和带着的目标",
        fields: &TASK_LIST_FIELDS,
    },
    Spec {
        action: Action::InterventionList,
        name: "intervention.list",
        read_only: true,
        summary: "列出指向**当前对话**、还没答复的待处理事项（只读）：停住的运行在对话里问过操作者的那些，\
                  每项带 handle、类型（blocked / confirm）、问的是什么、可答的结论（verdicts）",
        fields: &[],
    },
    Spec {
        action: Action::InterventionAnswer,
        name: "intervention.answer",
        read_only: false,
        summary: "替操作者提交对一个待处理事项的答复。**只有操作者在这一轮里明确回答了那个问题才能调用**：\
                  比如你在对话里问过要不要执行某个对外动作、操作者回了「发吧」，你读到上下文里那句提问和这句回复，\
                  才能 approve；操作者没说清楚就先问清楚，**不要替操作者做决定**。\
                  只有操作者亲自发消息开始的那一轮能用——定时、监听、任务交回触发的 Run 里一律被拒绝。\
                  只能答指向当前对话的事项；confirm 只认 approve / reject，blocked 的出错任务只认 abandon\
                  （崩溃停住的还认 resolve）；答过的再答返回原结论",
        fields: &[
            field(
                "handle",
                true,
                "待处理事项的 handle（对话里那句话的附注或 intervention.list 给的）",
            ),
            field(
                "verdict",
                true,
                "操作者的结论：approve（执行）/ reject（不执行）/ abandon（放弃）/ resolve（重试，仅崩溃停住的）",
            ),
        ],
    },
];

impl Action {
    fn spec(self) -> &'static Spec {
        SPECS
            .iter()
            .find(|spec| spec.action == self)
            .expect("每个动作都有说明")
    }

    pub fn name(self) -> &'static str {
        self.spec().name
    }

    /// 只读动作：可并发、可重跑。
    pub fn read_only(self) -> bool {
        self.spec().read_only
    }

    pub fn parse(raw: &str) -> Result<Action, ToolError> {
        let raw = raw.trim();
        SPECS
            .iter()
            .find(|spec| spec.name == raw)
            .map(|spec| spec.action)
            .ok_or_else(|| ToolError::InvalidArguments {
                message: format!(
                    "komo 不认识 action `{raw}`；可用：{}（各动作的参数见 tool://komo/doc）",
                    SPECS
                        .iter()
                        .map(|spec| spec.name)
                        .collect::<Vec<_>>()
                        .join("、")
                ),
            })
    }
}

/// 这个动作认的字段，写给模型看的一行：`content（必填）、kind、…`。
fn fields_line(spec: &Spec) -> String {
    if spec.fields.is_empty() {
        return "没有参数".into();
    }
    spec.fields
        .iter()
        .map(|field| match field.required {
            true => format!("{}（必填）", field.name),
            false => field.name.to_string(),
        })
        .collect::<Vec<_>>()
        .join("、")
}

/// 一次调用要 Gateway 做的事：已经带着身份、反序列化成了现成的请求类型。
#[derive(Debug, Clone, PartialEq)]
pub enum KomoCall {
    MemoryAdd(MemoryAddRequest),
    MemorySearch(MemoryListQuery),
    MemoryForget {
        id: MemoryId,
        request: MemoryRevisionRequest,
    },
    GoalAdd(MatterAddRequest),
    GoalList(MatterListQuery),
    /// 当前任务（调用的会话）挂到 / 摘下这个目标。
    GoalAttach {
        id: MatterId,
    },
    GoalDetach {
        id: MatterId,
    },
    GoalAct {
        id: MatterId,
        verb: MatterVerb,
        request: MatterActionRequest,
    },
    CronList,
    CronRun {
        job: CronJobId,
        request: ManualCronRunRequest,
    },
    SessionSearch(SessionSearchQuery),
    SessionList(SessionListQuery),
    TaskList(TaskListQuery),
    /// 指向调用它的那段对话、还没答复的介入。
    InterventionList,
    /// 替操作者答复一个介入。`session` / `run` 是调用它的会话与 Run（Gateway 据此核对这个 Run 是不是
    /// 操作者亲自发消息开始的，以及这条介入是不是指向这段对话）。
    InterventionAnswer {
        session: SessionId,
        run: RunId,
        handle: String,
        verdict: InterventionVerdict,
    },
}

/// Gateway 一侧：调 HTTP 路由背后的同一个处理函数，返回响应体的 JSON。
///
/// 处理函数的拒绝（"任务会话不能替用户记"、`memory = off` 的会话被拒、版本冲突 …）原样变成
/// [`ToolError`]，模型照着改。
#[async_trait]
pub trait KomoService: Send + Sync {
    /// `caller`：这次工具调用所在的会话（宿主填的身份）；`goal.attach` / `goal.detach` 改的是它自己的 `komo.task`。
    async fn call(&self, caller: &SessionId, call: KomoCall) -> Result<Value, ToolError>;
}

/// 工具与 Gateway 之间的空槽：工具先造好，服务等整份状态有了才接上。
#[derive(Default)]
pub struct KomoSlot(OnceLock<Arc<dyn KomoService>>);

impl std::fmt::Debug for KomoSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KomoSlot")
            .field("wired", &self.0.get().is_some())
            .finish()
    }
}

impl KomoSlot {
    pub fn set(&self, service: Arc<dyn KomoService>) {
        if self.0.set(service).is_err() {
            tracing::warn!("komo 工具的服务已经接过一次，忽略这次重复设置");
        }
    }

    pub fn get(&self) -> Option<&Arc<dyn KomoService>> {
        self.0.get()
    }
}

#[derive(Debug)]
pub struct KomoTool {
    slot: Arc<KomoSlot>,
}

impl KomoTool {
    pub fn new(slot: Arc<KomoSlot>) -> Self {
        Self { slot }
    }
}

/// 模型给的参数。
#[derive(serde::Deserialize)]
struct KomoArgs {
    action: String,
    #[serde(default)]
    params: Value,
}

/// `tool://komo/doc`：动作清单与字段表，从 [`SPECS`] 现算。
fn doc() -> String {
    let mut text = String::from(
        "`params` 是一个对象，字段按动作不同（下面逐个列出，必填的标明）；给了不认识的字段会被拒绝并列出该动作认的字段。\
         身份（哪个会话、哪个 Run）由宿主填，不要在 params 里给 session / run。\n\
         写动作按顺序一个个执行；只读动作（memory.search、goal.list、cron.list、session.*、task.list、intervention.list）可以和别的只读调用放同一轮并行。\n\
         改承诺的写动作（goal.add、goal.done）要这个 Run 里有用户说的话作依据；后台任务会话与定时任务会话被拒绝——\
         把要记的在交回的结果里写明，由主会话来做。\n\
         goal.attach / goal.detach 改的是任务自己的关联、不是目标的承诺，不要用户原话；但只有任务会话里能用。\
         intervention.answer 替操作者答复一个待处理事项（停住的运行在对话里问过他的问题）：只有他在这一轮里明确回答了\
         才能调用，不要替他做决定；触发器起的 Run（定时、监听、任务交回）里被拒绝。\n\
         不在这里的：确认记忆（用户自己的动作）、Gateway 的重启与停止。\n",
    );
    for spec in &SPECS {
        text.push_str(&format!(
            "\n## {}{}\n{}\n",
            spec.name,
            if spec.read_only { "（只读）" } else { "" },
            spec.summary
        ));
        for field in spec.fields {
            text.push_str(&format!(
                "- {}{}：{}\n",
                field.name,
                if field.required { "（必填）" } else { "" },
                field.note
            ));
        }
        if spec.fields.is_empty() {
            text.push_str("- 没有参数\n");
        }
    }
    text
}

/// 校验 `params` 并规整成存进计划的那份：对象、没有不认识的字段、必填的都在。**不带身份**——
/// 身份在 [`build`] 里加，计划哈希不依赖它。
fn normalize(action: Action, params: &Value) -> Result<Map<String, Value>, ToolError> {
    let spec = action.spec();
    let map = match params {
        Value::Null => Map::new(),
        Value::Object(map) => map.clone(),
        other => {
            return Err(wrong(spec, format!("params 要是一个对象，收到 {other}")));
        }
    };
    if let Some(unknown) = map
        .keys()
        .find(|key| !spec.fields.iter().any(|field| field.name == key.as_str()))
    {
        return Err(wrong(spec, format!("不认识的字段 `{unknown}`")));
    }
    for field in spec.fields.iter().filter(|field| field.required) {
        if map.get(field.name).is_none_or(Value::is_null) {
            return Err(wrong(spec, format!("缺少必填字段 `{}`", field.name)));
        }
    }
    Ok(map)
}

/// `params` 不对：说哪里不对，再列出这个动作认的字段。
fn wrong(spec: &Spec, why: impl std::fmt::Display) -> ToolError {
    ToolError::InvalidArguments {
        message: format!(
            "{} 的 params 不合法：{why}。这个动作认的字段：{}（说明见 tool://komo/doc）",
            spec.name,
            fields_line(spec)
        ),
    }
}

fn decode<T: DeserializeOwned>(spec: &Spec, map: Map<String, Value>) -> Result<T, ToolError> {
    serde_json::from_value(Value::Object(map)).map_err(|error| wrong(spec, error))
}

fn text_of(spec: &Spec, map: &mut Map<String, Value>, key: &str) -> Result<String, ToolError> {
    match map.remove(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        Some(Value::String(_)) | None => Err(wrong(spec, format!("`{key}` 不能是空的"))),
        Some(other) => Err(wrong(spec, format!("`{key}` 要是字符串，收到 {other}"))),
    }
}

/// 把规整后的参数加上身份、反序列化成这个动作的请求类型。
fn build(
    action: Action,
    params: &Map<String, Value>,
    ctx: &ToolContext,
) -> Result<KomoCall, ToolError> {
    let spec = action.spec();
    let mut map = params.clone();
    // 模型写 `revision`，请求类型认 `expected_revision`。
    if let Some(revision) = map.remove("revision") {
        map.insert("expected_revision".into(), revision);
    }
    let key = RequestKey::new(format!("komo:{}", ctx.call));
    let identity = |map: &mut Map<String, Value>| {
        map.insert("session".into(), json!(ctx.session));
        // `bash` 子进程的 `KOMO_RUN` 也是输入 Run：Gateway 据此找这个 Run 里用户的原话。
        map.insert(
            "run".into(),
            json!(ctx.input_run.as_ref().unwrap_or(&ctx.run)),
        );
        map.insert("request_key".into(), json!(key));
    };
    let matter = |map: &mut Map<String, Value>, verb: MatterVerb| -> Result<KomoCall, ToolError> {
        let id = MatterId::from_raw(text_of(spec, map, "id")?);
        map.insert("kind".into(), json!(MatterKind::Goal));
        identity(map);
        Ok(KomoCall::GoalAct {
            id,
            verb,
            request: decode(spec, std::mem::take(map))?,
        })
    };
    match action {
        Action::MemoryAdd => {
            map.entry("kind").or_insert_with(|| json!("preference"));
            // 作用域在 JSON 里是带 `kind` 的对象；模型写文本（`project:komo`）更自然，两种都收。
            let scope = match map.remove("scope") {
                None => MemoryScope::Personal,
                Some(Value::String(text)) => text
                    .parse::<MemoryScope>()
                    .map_err(|error| wrong(spec, error))?,
                Some(other) => serde_json::from_value(other).map_err(|error| wrong(spec, error))?,
            };
            map.insert("scope".into(), json!(scope));
            identity(&mut map);
            Ok(KomoCall::MemoryAdd(decode(spec, map)?))
        }
        Action::MemorySearch => {
            let query = text_of(spec, &mut map, "query")?;
            map.insert("query".into(), json!(query));
            // 带会话 = Agent 在 Run 里自己查：Gateway 按这个会话的 Profile 放行作用域。
            map.insert("session".into(), json!(ctx.session));
            Ok(KomoCall::MemorySearch(decode(spec, map)?))
        }
        Action::MemoryForget => {
            let id = MemoryId::from_raw(text_of(spec, &mut map, "id")?);
            map.insert("request_key".into(), json!(key));
            Ok(KomoCall::MemoryForget {
                id,
                request: decode(spec, map)?,
            })
        }
        Action::GoalAdd => {
            map.insert("kind".into(), json!(MatterKind::Goal));
            identity(&mut map);
            Ok(KomoCall::GoalAdd(decode(spec, map)?))
        }
        Action::GoalList => {
            map.insert("kind".into(), json!(MatterKind::Goal));
            // 带会话：`memory = private / off` 的会话被拒，同路由。
            map.insert("session".into(), json!(ctx.session));
            Ok(KomoCall::GoalList(decode(spec, map)?))
        }
        Action::GoalAttach => Ok(KomoCall::GoalAttach {
            id: MatterId::from_raw(text_of(spec, &mut map, "id")?),
        }),
        Action::GoalDetach => Ok(KomoCall::GoalDetach {
            id: MatterId::from_raw(text_of(spec, &mut map, "id")?),
        }),
        Action::GoalNext => matter(&mut map, MatterVerb::Next),
        Action::GoalWait => matter(&mut map, MatterVerb::Wait),
        Action::GoalResume => matter(&mut map, MatterVerb::Resume),
        Action::GoalDone => matter(&mut map, MatterVerb::Done),
        Action::GoalLink => {
            let trigger = match (map.remove("cron"), map.remove("monitor")) {
                (Some(Value::String(job)), None) => {
                    MatterTrigger::cron(CronJobId::from_raw(job.trim()))
                }
                (None, Some(Value::String(monitor))) => {
                    MatterTrigger::parse_monitor(&monitor).map_err(|why| wrong(spec, why))?
                }
                _ => {
                    return Err(wrong(
                        spec,
                        "要给 cron（Job id）或 monitor（SESSION_ID:MONITOR_ID）其中一个，且是字符串",
                    ));
                }
            };
            trigger.check_shape().map_err(|why| wrong(spec, why))?;
            map.insert("trigger".into(), json!(trigger));
            matter(&mut map, MatterVerb::Link)
        }
        Action::CronList => Ok(KomoCall::CronList),
        Action::CronRun => {
            let job = CronJobId::from_raw(text_of(spec, &mut map, "id")?);
            Ok(KomoCall::CronRun {
                job,
                request: ManualCronRunRequest {
                    request_key: Some(key),
                },
            })
        }
        Action::SessionSearch => Ok(KomoCall::SessionSearch(decode(spec, map)?)),
        Action::SessionList => Ok(KomoCall::SessionList(decode(spec, map)?)),
        Action::TaskList => Ok(KomoCall::TaskList(decode(spec, map)?)),
        Action::InterventionList => Ok(KomoCall::InterventionList),
        Action::InterventionAnswer => {
            let handle = text_of(spec, &mut map, "handle")?;
            let verdict = text_of(spec, &mut map, "verdict")?;
            let verdict =
                serde_json::from_value::<InterventionVerdict>(json!(verdict)).map_err(|_| {
                    wrong(
                        spec,
                        "`verdict` 要是 approve / reject / abandon / resolve 之一",
                    )
                })?;
            Ok(KomoCall::InterventionAnswer {
                session: ctx.session.clone(),
                run: ctx.input_run.clone().unwrap_or_else(|| ctx.run.clone()),
                handle,
                verdict,
            })
        }
    }
}

#[async_trait]
impl Tool for KomoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: KOMO_TOOL.into(),
            description: "操作 komo 自己的状态，不经 shell：记忆（memory.add / search / forget）、\
                          目标（goal.add / list / next / wait / resume / done / link / attach / detach）、定时任务（cron.list / run）、\
                          找会话（session.search / list）、任务清单（task.list）、\
                          待处理事项（intervention.list / answer：操作者在对话里答复了你问过的问题，才替他提交）。\
                          用户要你记住什么、提到别的会话里的事、问有哪些定时任务或任务时用它，\
                          别去翻系统 crontab 或读数据文件。\n\
                          用前先 read tool://komo/doc（各动作的 params 字段）。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": SPECS.iter().map(|spec| spec.name).collect::<Vec<_>>(),
                        "description": "领域.动作"
                    },
                    "params": {
                        "type": "object",
                        "description": "这个动作的参数，字段见 tool://komo/doc；没有参数的动作省略"
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            doc: Some(doc()),
        }
    }

    async fn prepare(&self, args: Value, ctx: &ToolContext) -> Result<ExecutionPlan, ToolError> {
        let args: KomoArgs = parse_args(args, KOMO_TOOL)?;
        let action = Action::parse(&args.action)?;
        let params = normalize(action, &args.params)?;
        // 在这里就反序列化一遍：字段的类型、枚举值、日期格式填错，当场报错，而不是落了计划才失败。
        build(action, &params, ctx)?;
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: KOMO_TOOL.into(),
            operation: Operation::Komo {
                action: action.name().into(),
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: json!({ "action": action.name(), "params": params }),
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            recovery: if action.read_only() {
                // 只读：重做只是重新观察。
                RecoveryMode::SafeReread
            } else {
                RecoveryMode::NoSafeRecovery
            },
            then_run: None,
        })
    }

    async fn execute(
        &self,
        plan: ExecutionPlan,
        ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let args: KomoArgs =
            serde_json::from_value(plan.args.clone()).map_err(|error| ToolError::Failed {
                message: format!("komo 的计划参数读不出来：{error}"),
            })?;
        let action = Action::parse(&args.action)?;
        let params = normalize(action, &args.params)?;
        let call = build(action, &params, ctx)?;
        let service = self.slot.get().ok_or_else(|| ToolError::Failed {
            message: "这台 Gateway 没有接上 komo 的状态服务，komo 工具用不了".into(),
        })?;
        let result = service.call(&ctx.session, call).await?;
        let mut preview = serde_json::to_string(&result).map_err(|error| ToolError::Failed {
            message: format!("结果无法序列化：{error}"),
        })?;
        if action == Action::MemorySearch {
            // 宿主标记：查到的是已有记忆，不是新证据；记忆提取认输出里的这一行
            // （`MEMORY_QUERY_MARKER`），不让它被引为证据。
            preview = format!("{MEMORY_QUERY_MARKER}\n{preview}");
        }
        Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result,
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            preview: Some(preview),
        })
    }

    /// 声明 `Safe` 只为让只读动作的计划（`SafeReread`）能在重启后重跑；写动作的计划是
    /// `NoSafeRecovery`，两边都 `Safe` 才重跑，所以它们不会被重放。
    fn replay(&self) -> Replay {
        Replay::Safe
    }

    fn parallel_for(&self, args: &Value) -> bool {
        args.get("action")
            .and_then(Value::as_str)
            .and_then(|action| Action::parse(action).ok())
            .is_some_and(Action::read_only)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::context;

    /// 记下收到的调用、回一个固定结果的服务。
    #[derive(Default)]
    struct Recorder {
        calls: std::sync::Mutex<Vec<KomoCall>>,
    }

    #[async_trait]
    impl KomoService for Recorder {
        async fn call(&self, _caller: &SessionId, call: KomoCall) -> Result<Value, ToolError> {
            self.calls.lock().unwrap().push(call);
            Ok(json!({ "ok": true }))
        }
    }

    fn tool() -> (KomoTool, Arc<Recorder>) {
        let slot = Arc::new(KomoSlot::default());
        let recorder = Arc::new(Recorder::default());
        slot.set(recorder.clone());
        (KomoTool::new(slot), recorder)
    }

    async fn run_action(tool: &KomoTool, args: Value) -> Result<ToolOutput, ToolError> {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let plan = tool.prepare(args, &ctx).await?;
        tool.execute(plan, &ctx, &mut crate::tools::test_support::writer(&ctx))
            .await
    }

    #[tokio::test]
    async fn every_action_prepares_into_the_typed_call_with_the_identity_of_the_context() {
        let (tool, recorder) = tool();
        let cases = [
            ("memory.add", json!({ "content": "空调 26 度" })),
            ("memory.search", json!({ "query": "空调" })),
            ("memory.forget", json!({ "id": "m-1", "revision": 2 })),
            ("goal.add", json!({ "title": "发布观察" })),
            ("goal.next", json!({ "id": "g-1", "next": "看 5xx" })),
            ("goal.wait", json!({ "id": "g-1", "waiting_for": "审批" })),
            ("goal.resume", json!({ "id": "g-1" })),
            ("goal.done", json!({ "id": "g-1", "revision": 3 })),
            ("goal.link", json!({ "id": "g-1", "cron": "job-1" })),
            ("goal.list", json!({ "all": true })),
            ("goal.attach", json!({ "id": "g-1" })),
            ("goal.detach", json!({ "id": "g-1" })),
            ("cron.list", json!({})),
            ("cron.run", json!({ "id": "job-1" })),
            ("session.search", json!({ "q": "飞书" })),
            ("session.list", json!({ "all": true, "limit": 5 })),
            ("task.list", json!({})),
            ("intervention.list", json!({})),
            (
                "intervention.answer",
                json!({ "handle": "42", "verdict": "approve" }),
            ),
        ];
        assert_eq!(cases.len(), SPECS.len(), "每个动作都有一条用例");
        for (action, params) in cases {
            run_action(&tool, json!({ "action": action, "params": params }))
                .await
                .unwrap_or_else(|error| panic!("{action}：{error}"));
        }
        let calls = recorder.calls.lock().unwrap();
        assert_eq!(calls.len(), SPECS.len());

        let KomoCall::MemoryAdd(add) = &calls[0] else {
            panic!("{:?}", calls[0])
        };
        assert_eq!(add.scope, MemoryScope::Personal, "没给 scope 记成 personal");
        assert_eq!(add.session.as_ref().unwrap().as_str(), "sess-1");
        assert_eq!(add.run.as_ref().unwrap().as_str(), "run-1");
        assert_eq!(add.request_key.as_ref().unwrap().as_str(), "komo:call-1");
        let KomoCall::MemorySearch(search) = &calls[1] else {
            panic!("{:?}", calls[1])
        };
        assert_eq!(search.session.as_ref().unwrap().as_str(), "sess-1");
        let KomoCall::MemoryForget { id, request } = &calls[2] else {
            panic!("{:?}", calls[2])
        };
        assert_eq!((id.as_str(), request.expected_revision), ("m-1", 2));
        let KomoCall::GoalAct { verb, request, .. } = &calls[8] else {
            panic!("{:?}", calls[8])
        };
        assert_eq!(*verb, MatterVerb::Link);
        assert_eq!(request.kind, Some(MatterKind::Goal));
        assert_eq!(
            request.trigger,
            Some(MatterTrigger::cron(CronJobId::from_raw("job-1")))
        );
        // goal.list：只列目标，带会话（同路由的闸）。
        let KomoCall::GoalList(list) = &calls[9] else {
            panic!("{:?}", calls[9])
        };
        assert_eq!(list.kind, Some(MatterKind::Goal));
        assert!(list.all);
        assert_eq!(list.session.as_ref().unwrap().as_str(), "sess-1");
        assert_eq!(
            calls[10],
            KomoCall::GoalAttach {
                id: MatterId::from_raw("g-1")
            }
        );
        assert_eq!(
            calls[11],
            KomoCall::GoalDetach {
                id: MatterId::from_raw("g-1")
            }
        );
    }

    #[tokio::test]
    async fn task_list_takes_a_goal_filter() {
        let (tool, recorder) = tool();
        run_action(
            &tool,
            json!({ "action": "task.list", "params": { "goal": "g-7", "limit": 3 } }),
        )
        .await
        .unwrap();
        let calls = recorder.calls.lock().unwrap();
        let KomoCall::TaskList(query) = &calls[0] else {
            panic!("{:?}", calls[0])
        };
        assert_eq!(query.goal.as_ref().map(|goal| goal.as_str()), Some("g-7"));
        assert_eq!(query.limit, Some(3));
    }

    #[tokio::test]
    async fn a_memory_scope_is_accepted_as_text_and_the_run_is_the_input_run() {
        let (tool, recorder) = tool();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.input_run = Some(komo_kernel::types::ids::RunId::from_raw("input-9"));
        let plan = tool
            .prepare(
                json!({ "action": "memory.add", "params": {
                    "content": "komo 用 Rust 写", "kind": "fact", "scope": "project:komo" } }),
                &ctx,
            )
            .await
            .unwrap();
        tool.execute(plan, &ctx, &mut crate::tools::test_support::writer(&ctx))
            .await
            .unwrap();
        let calls = recorder.calls.lock().unwrap();
        let KomoCall::MemoryAdd(add) = &calls[0] else {
            panic!("{:?}", calls[0])
        };
        assert_eq!(
            add.scope,
            MemoryScope::Project {
                project_id: "komo".into()
            }
        );
        assert_eq!(add.run.as_ref().unwrap().as_str(), "input-9");
    }

    #[tokio::test]
    async fn wrong_params_are_refused_listing_the_fields_the_action_takes() {
        let (tool, _) = tool();
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        for (args, expect) in [
            (
                json!({ "action": "memory.add", "params": { "text": "x" } }),
                "不认识的字段 `text`",
            ),
            (
                json!({ "action": "memory.add", "params": {} }),
                "缺少必填字段 `content`",
            ),
            (
                json!({ "action": "memory.add", "params": { "content": "x", "kind": "idea" } }),
                "idea",
            ),
            (
                json!({ "action": "goal.add", "params": { "title": "x", "due": "明天" } }),
                "due",
            ),
            (
                // 身份由宿主填：模型给的 session 不是字段。
                json!({ "action": "goal.add", "params": { "title": "x", "session": "s-2" } }),
                "不认识的字段 `session`",
            ),
            (
                json!({ "action": "goal.link", "params": { "id": "g-1" } }),
                "cron",
            ),
            (
                json!({ "action": "goal.link", "params": { "id": "g-1", "cron": "a", "monitor": "s:m" } }),
                "monitor",
            ),
            (
                json!({ "action": "cron.list", "params": "x" }),
                "要是一个对象",
            ),
        ] {
            let error = tool.prepare(args.clone(), &ctx).await.unwrap_err();
            let ToolError::InvalidArguments { message } = &error else {
                panic!("{args}：{error:?}")
            };
            assert!(message.contains(expect), "{args}：{message}");
            let action = args["action"].as_str().unwrap();
            let spec = Action::parse(action).unwrap().spec();
            assert!(
                message.contains(&fields_line(spec)),
                "报错要列出 {action} 认的字段：{message}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_action_is_refused_listing_the_actions() {
        let (tool, _) = tool();
        let dir = tempfile::tempdir().unwrap();
        for action in [
            "memory.confirm",
            "area.add",
            "gateway.restart",
            "memory",
            "",
        ] {
            let error = tool
                .prepare(json!({ "action": action }), &context(dir.path()))
                .await
                .unwrap_err();
            let ToolError::InvalidArguments { message } = &error else {
                panic!("{action}：{error:?}")
            };
            assert!(
                message.contains("memory.add") && message.contains("task.list"),
                "{message}"
            );
        }
    }

    #[tokio::test]
    async fn only_the_read_actions_are_replayable_and_parallel() {
        let (tool, _) = tool();
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        assert_eq!(tool.replay(), Replay::Safe);
        let params = |action: &str| match action {
            "memory.add" => json!({ "content": "x" }),
            "memory.search" => json!({ "query": "x" }),
            "memory.forget" => json!({ "id": "m", "revision": 1 }),
            "goal.add" => json!({ "title": "x" }),
            "goal.next" => json!({ "id": "g", "next": "x" }),
            "goal.wait" => json!({ "id": "g", "waiting_for": "x" }),
            "goal.resume" | "goal.done" | "goal.attach" | "goal.detach" => json!({ "id": "g" }),
            "goal.link" => json!({ "id": "g", "cron": "j" }),
            "cron.run" => json!({ "id": "j" }),
            "session.search" => json!({ "q": "x" }),
            "intervention.answer" => json!({ "handle": "7", "verdict": "approve" }),
            _ => json!({}),
        };
        for spec in &SPECS {
            let args = json!({ "action": spec.name, "params": params(spec.name) });
            let plan = tool.prepare(args.clone(), &ctx).await.unwrap();
            assert_eq!(
                plan.recovery.replay() == Replay::Safe,
                spec.read_only,
                "{} 的计划",
                spec.name
            );
            assert_eq!(tool.parallel_for(&args), spec.read_only, "{}", spec.name);
            let Operation::Komo { action } = &plan.operation else {
                panic!("{:?}", plan.operation)
            };
            assert_eq!(action, spec.name);
            assert!(plan.targets.is_empty());
        }
        // 只读动作是这些，不多不少。
        let reads: Vec<&str> = SPECS
            .iter()
            .filter(|spec| spec.read_only)
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            reads,
            [
                "memory.search",
                "goal.list",
                "cron.list",
                "session.search",
                "session.list",
                "task.list",
                "intervention.list"
            ]
        );
        // 认不出来的动作不算只读。
        assert!(!tool.parallel_for(&json!({ "action": "nope" })));
        assert!(!tool.parallel_for(&json!({})));
    }

    #[tokio::test]
    async fn a_memory_search_result_carries_the_host_marker_so_it_is_not_new_evidence() {
        let (tool, _) = tool();
        let output = run_action(
            &tool,
            json!({ "action": "memory.search", "params": { "query": "空调" } }),
        )
        .await
        .unwrap();
        let preview = output.preview.unwrap();
        assert!(preview.starts_with(MEMORY_QUERY_MARKER), "{preview}");
        let other = run_action(&tool, json!({ "action": "cron.list" }))
            .await
            .unwrap();
        assert!(!other.preview.unwrap().contains(MEMORY_QUERY_MARKER));
    }

    #[tokio::test]
    async fn an_unwired_tool_fails_cleanly() {
        let tool = KomoTool::new(Arc::new(KomoSlot::default()));
        let error = run_action(&tool, json!({ "action": "cron.list" }))
            .await
            .unwrap_err();
        let ToolError::Failed { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("komo"), "{message}");
    }

    #[test]
    fn the_schema_lists_every_action_and_the_doc_lists_every_field() {
        let (tool, _) = tool();
        let definition = tool.definition();
        let listed: Vec<&str> = definition.parameters["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(listed.len(), SPECS.len());
        assert!(
            definition.parameters["properties"]["params"]
                .get("properties")
                .is_none(),
            "各动作的字段不进 schema"
        );
        let doc = definition.doc.unwrap();
        for spec in &SPECS {
            assert!(doc.contains(&format!("## {}", spec.name)), "{}", spec.name);
            for field in spec.fields {
                assert!(doc.contains(field.name), "{}.{}", spec.name, field.name);
            }
        }
        for absent in ["memory.confirm", "area."] {
            assert!(!listed.iter().any(|name| name.starts_with(absent)));
        }
    }
}
