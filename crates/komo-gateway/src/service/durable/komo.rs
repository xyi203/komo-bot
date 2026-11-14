//! `komo` 工具的 Gateway 一侧（`docs/tasks.md` §4.4）：[`KomoService`] 的实现。
//!
//! 每个动作调 HTTP 路由背后的**同一个处理函数**（`http::memories` / `matters` / `cron` /
//! `v2::sessions` / `v2::tasks`），不另写业务逻辑，也不经 HTTP 回环：规则（改承诺要这个 Run 里
//! 用户的原话、任务会话与 Cron 被拒、`memory = off` 的会话被拒、幂等键、版本冲突 …）只有这一份，
//! 工具与路由自然一致。处理函数要一个 [`Api`]，所以这里持有它的几块（幂等表、web 策略），
//! `GatewayState` 只握弱引用——`GatewayState` → 工具目录 → 这个服务不能再绕回去成环。

use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use komo_kernel::protocol::http::ErrorCode;
use komo_kernel::types::ids::{MatterId, SessionId};
use komo_kernel::types::matter::MatterKind;
use komo_kernel::types::tool::ToolError;
use komo_runtime::durable::harness::orchestration::{load_task_doc, store_task_doc};
use komo_tools::tools::komo::{KomoCall, KomoService};
use serde::Serialize;
use serde_json::{Value, json};

use crate::http::error::{ApiFailure, ApiResult};
use crate::http::idempotency::Idempotency;
use crate::http::v2::{sessions, tasks};
use crate::http::{Api, matters, memories};
use crate::service::durable::sessions::SessionError;
use crate::service::state::GatewayState;
use crate::web::WebAuth;

pub struct GatewayKomo {
    state: Weak<GatewayState>,
    idempotency: Arc<Idempotency>,
    web: Arc<WebAuth>,
}

impl GatewayKomo {
    pub fn new(state: &Arc<GatewayState>) -> Arc<Self> {
        // 处理函数用不到 web 策略，只是 `Api` 要有它；监听地址在这里无意义。
        let api = Api::new(Arc::clone(state), SocketAddr::from(([127, 0, 0, 1], 0)));
        Arc::new(GatewayKomo {
            state: Arc::downgrade(state),
            idempotency: api.idempotency,
            web: api.web,
        })
    }

    fn api(&self) -> Result<Api, ToolError> {
        let state = self.state.upgrade().ok_or_else(|| ToolError::Failed {
            message: "Gateway 正在停机，komo 的状态服务已经不在了".into(),
        })?;
        Ok(Api {
            state,
            idempotency: Arc::clone(&self.idempotency),
            web: Arc::clone(&self.web),
        })
    }
}

#[async_trait]
impl KomoService for GatewayKomo {
    async fn call(&self, caller: &SessionId, call: KomoCall) -> Result<Value, ToolError> {
        let api = self.api()?;
        match call {
            KomoCall::MemoryAdd(request) => reply(memories::add(State(api), Json(request)).await),
            KomoCall::MemorySearch(query) => reply(memories::list(State(api), Query(query)).await),
            KomoCall::MemoryForget { id, request } => {
                reply(memories::forget(State(api), Path(id.to_string()), Json(request)).await)
            }
            KomoCall::GoalAdd(request) => reply(matters::add(State(api), Json(request)).await),
            KomoCall::GoalList(query) => goal_list(api, query).await,
            KomoCall::GoalAttach { id } => set_task_goal(&api, caller, id, true).await,
            KomoCall::GoalDetach { id } => set_task_goal(&api, caller, id, false).await,
            KomoCall::GoalAct { id, verb, request } => reply(
                matters::act(
                    State(api),
                    Path((id.to_string(), verb.as_str().to_string())),
                    Json(request),
                )
                .await,
            ),
            KomoCall::SessionSearch(query) => {
                reply(sessions::search(State(api), Query(query)).await)
            }
            KomoCall::SessionList(query) => reply(
                sessions::list(
                    State(api),
                    list_uri(query.all),
                    Query(sessions::ListPaging {
                        limit: query.limit,
                        cursor: query.cursor,
                    }),
                )
                .await,
            ),
            KomoCall::InterventionList => intervention_list(&api, caller).await,
            KomoCall::InterventionAnswer {
                session,
                run,
                handle,
                verdict,
            } => intervention_answer(&api, &session, &run, &handle, verdict).await,
            KomoCall::TaskList(query) => reply(
                tasks::list(
                    State(api),
                    list_uri(query.all),
                    Query(tasks::TaskPaging {
                        limit: query.limit,
                        cursor: query.cursor,
                        goal: query.goal,
                    }),
                )
                .await,
            ),
        }
    }
}

/// `intervention.list`：指向调用它的那段对话、还没答复的介入（只读）。任务会话与定时会话不是对话，
/// 没有指向它们的介入，清单是空的。
async fn intervention_list(api: &Api, caller: &SessionId) -> Result<Value, ToolError> {
    let core = api.state.core().map_err(|error| refusal(error.into()))?;
    let interventions = core
        .interventions_for_chat(caller.as_str())
        .await
        .map_err(session_failure)?;
    Ok(json!({ "interventions": interventions }))
}

/// `intervention.answer`：替操作者答复一个介入。只有操作者亲自发消息开始的 Run 能答，规则与理由见
/// [`crate::service::durable::chats::attention`]。
async fn intervention_answer(
    api: &Api,
    session: &SessionId,
    run: &komo_kernel::types::ids::RunId,
    handle: &str,
    verdict: komo_kernel::protocol::http::InterventionVerdict,
) -> Result<Value, ToolError> {
    use crate::service::durable::chats::attention::ChatAnswerError;
    let core = api.state.core().map_err(|error| refusal(error.into()))?;
    let response = core
        .answer_from_chat(session.as_str(), run, handle.trim(), verdict)
        .await
        .map_err(|error| match error {
            ChatAnswerError::Denied(reason) => ToolError::Denied { reason },
            ChatAnswerError::Invalid(message) => ToolError::InvalidArguments { message },
            ChatAnswerError::Failed(message) => ToolError::Failed { message },
        })?;
    serde_json::to_value(response).map_err(|error| ToolError::Failed {
        message: format!("响应无法序列化：{error}"),
    })
}

/// `goal.list`：路由的清单，每个目标再带上 `tasks`——有几个（活着的）任务带着它的键。计数不是目标自己的
/// 字段（目标落在 aux.db、任务的键落在 durable.db），所以在这里补到响应里，路由与线上的 `Matter` 类型不变。
async fn goal_list(
    api: Api,
    query: komo_kernel::protocol::http::MatterListQuery,
) -> Result<Value, ToolError> {
    let core = api.state.core().map_err(|error| refusal(error.into()))?;
    let counts = core.goal_task_counts().await.map_err(session_failure)?;
    let mut value = reply(matters::list(State(api), Query(query)).await)?;
    if let Some(items) = value.get_mut("matters").and_then(Value::as_array_mut) {
        for item in items {
            let count = item
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| counts.get(id))
                .copied()
                .unwrap_or(0);
            item["tasks"] = count.into();
        }
    }
    Ok(value)
}

/// `goal.attach` / `goal.detach`：改**调用它的任务自己**的 `komo.task.goals`（`docs/goals.md` 的"任务与目标的关联"）。
///
/// 这是任务自己的关联、不是目标的承诺，所以不走 `matters::act`、不要 Run 里的用户原话（只由监听事件起的一轮也能用）。
/// 要求：不是 `memory = private / off` 的 Agent（同目标的所有读写，[`matters::gate`]）；当前会话是任务会话（有
/// `komo.task`）。目标完成或放弃不会清掉任务上的键——
/// 那是有意的：历史关联仍有意义（`TaskDoc::goals`）。
async fn set_task_goal(
    api: &Api,
    caller: &SessionId,
    goal: MatterId,
    attach: bool,
) -> Result<Value, ToolError> {
    matters::gate(api, caller).await.map_err(refusal)?;
    let core = api.state.core().map_err(|error| refusal(error.into()))?;
    let record = core
        .session(caller.as_str())
        .await
        .map_err(session_failure)?;
    let not_a_task = || ToolError::InvalidArguments {
        message: "goal.attach / goal.detach 只有任务里能用：当前会话不是任务会话".into(),
    };
    if record.kind != komo_kernel::durable::SessionKind::Task {
        return Err(not_a_task());
    }
    // 挂的时候目标必须存在且是目标；摘的时候不查，目标没了也要能摘掉。
    let title =
        if attach {
            let matter = api.state.matters.get(&goal).await.map_err(|error| {
                ToolError::InvalidArguments {
                    message: error.to_string(),
                }
            })?;
            if matter.kind != MatterKind::Goal {
                return Err(ToolError::InvalidArguments {
                    message: format!("{goal} 是领域，不是目标；任务只能挂到目标上"),
                });
            }
            Some(matter.title)
        } else {
            None
        };
    let root = record.root_conversation;
    let id = goal.as_str().to_owned();
    let changed = core
        .engine
        .commit(move |tx| {
            Box::pin(async move {
                let Some(mut doc) = load_task_doc(tx, root).await? else {
                    return Ok(None);
                };
                let present = doc.goals.contains(&id);
                if present == attach {
                    return Ok(Some(false));
                }
                if attach {
                    doc.goals.push(id);
                } else {
                    doc.goals.retain(|existing| *existing != id);
                }
                store_task_doc(tx, root, &doc).await?;
                Ok(Some(true))
            })
        })
        .await
        .map_err(|error| ToolError::Failed {
            message: format!("写不了任务的目标关联：{error}"),
        })?;
    let Some(changed) = changed else {
        return Err(not_a_task());
    };
    Ok(json!({
        "goal": { "id": goal, "title": title },
        "attached": attach,
        "changed": changed,
    }))
}

fn session_failure(error: SessionError) -> ToolError {
    ToolError::Failed {
        message: error.to_string(),
    }
}

/// 会话与任务清单的处理函数从 URI 的 query 里读 `all`（它认 `?all`、`?all=1` 等几种写法）。
fn list_uri(all: bool) -> axum::http::Uri {
    match all {
        true => "/?all=true",
        false => "/",
    }
    .parse()
    .expect("固定的 URI")
}

fn reply<T: Serialize>(result: ApiResult<Json<T>>) -> Result<Value, ToolError> {
    let Json(body) = result.map_err(refusal)?;
    serde_json::to_value(body).map_err(|error| ToolError::Failed {
        message: format!("响应无法序列化：{error}"),
    })
}

/// 处理函数的拒绝变成工具错误：消息原样交给模型。403 是"被拒绝"（`memory = off`、给别人用的
/// Agent），请求本身不成立（含"任务会话不能替用户记 / 改承诺要用户原话"）是参数问题，其余是失败。
fn refusal(failure: ApiFailure) -> ToolError {
    let message = failure.error.message;
    match failure.error.code {
        ErrorCode::Denied => ToolError::Denied { reason: message },
        ErrorCode::InvalidRequest => ToolError::InvalidArguments { message },
        _ => ToolError::Failed { message },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use komo_kernel::durable::{SessionKind, TaskId};
    use komo_kernel::traits::Tool;
    use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::tool::{CancelToken, ToolContext, ToolOutput, WorkspaceRoot};
    use komo_runtime::durable::harness::orchestration::{TaskDoc, store_task_doc};
    use serde_json::json;

    use komo_kernel::durable::SessionRecord;

    use super::*;
    use crate::service::durable::sessions::NewSession;
    use crate::service::test_support::harness::{FakeLlm, Gw, Home};

    /// 开着记忆（只走关键词检索，不要向量端点）的配置；后台提取的空闲门槛放到一小时，测试里不会触发。
    const CONFIG: &str = r#"
default_agent = "assistant"

[agents.assistant]

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = true
model = "main"
extract_idle = "1h"

[memory.retrieval]
mode = "keyword"
"#;

    async fn start() -> (Home, Gw) {
        let home = Home::with_config(CONFIG);
        let gateway = home.start(FakeLlm::finisher("好")).await;
        (home, gateway)
    }

    fn komo_tool(gateway: &Gw) -> Arc<dyn Tool> {
        gateway
            .state()
            .tool_executor
            .tools()
            .get("komo")
            .cloned()
            .expect("komo 在工具目录里")
    }

    /// 一次工具调用的上下文：身份是给定的会话与输入 Run。
    fn context(session: &SessionId, run: &RunId, source: PlanSource) -> ToolContext {
        let dir = std::env::temp_dir();
        ToolContext {
            session: session.clone(),
            run: RunId::from_raw("artifact-namespace"),
            input_run: Some(run.clone()),
            call: ToolCallId::from_raw("call-1"),
            attempt: AttemptId::from_raw("attempt-1"),
            source,
            cwd: dir.clone(),
            mounts: Default::default(),
            roots: vec![WorkspaceRoot {
                path: dir,
                writable: true,
                label: "workspace".into(),
            }],
            resumed: None,
            cancel: CancelToken::new(),
            call_timeout: std::time::Duration::from_secs(30),
            detach_after: None,
            interjected: Default::default(),
            conversation: true,
        }
    }

    /// 走完 `prepare` → `execute`，调用号每次不同（请求键取它）。
    async fn call(
        tool: &Arc<dyn Tool>,
        ctx: &mut ToolContext,
        action: &str,
        params: Value,
    ) -> Result<ToolOutput, ToolError> {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ctx.call = ToolCallId::from_raw(format!("call-{n}"));
        let plan = tool
            .prepare(json!({ "action": action, "params": params }), ctx)
            .await?;
        let mut sink =
            komo_kernel::test_support::MemOutputWriter::new(komo_kernel::types::refs::AttemptRef {
                session: ctx.session.clone(),
                run: ctx.run.clone(),
                call: ctx.call.clone(),
                attempt: ctx.attempt.clone(),
            });
        tool.execute(plan, ctx, &mut sink).await
    }

    async fn ok(tool: &Arc<dyn Tool>, ctx: &mut ToolContext, action: &str, params: Value) -> Value {
        match call(tool, ctx, action, params).await {
            Ok(output) => output.result,
            Err(error) => panic!("{action}：{error}"),
        }
    }

    /// 一个跑完了的主会话 Run，用户在里面说了一句话（改承诺要有这句话）。
    async fn user_run(gateway: &Gw, text: &str) -> (SessionId, RunId) {
        let session = gateway.open_session().await;
        let run = gateway.submit(&session, "komo-1", text).await.run;
        gateway.wait_terminal(&run).await;
        (session, run)
    }

    /// 十七个动作（`intervention.*` 两个另有专门的测试）各走通一次，经由真的处理函数（真 aux.db / durable.db，不是桩）：返回的是各自
    /// 路由的响应体，状态确实落了库。
    #[tokio::test]
    async fn every_komo_action_runs_through_the_real_handlers() {
        let (_home, gateway) = start().await;
        let tool = komo_tool(&gateway);
        let (session, run) = user_run(
            &gateway,
            "记住客厅空调设 26 度；开始做空调巡检，下一步看温度",
        )
        .await;
        let mut ctx = context(
            &session,
            &run,
            PlanSource::Interactive {
                session: session.clone(),
            },
        );

        // ── memory
        let added = ok(
            &tool,
            &mut ctx,
            "memory.add",
            json!({ "content": "客厅空调设 26 度", "kind": "preference" }),
        )
        .await;
        assert_eq!(added["created"], true, "{added}");
        let memory = added["memory"]["id"].as_str().unwrap().to_string();
        let revision = added["memory"]["revision"].as_u64().unwrap();
        let found = ok(
            &tool,
            &mut ctx,
            "memory.search",
            json!({ "query": "空调", "mode": "keyword" }),
        )
        .await;
        assert_eq!(found["memories"][0]["id"], memory.as_str(), "{found}");
        let forgotten = ok(
            &tool,
            &mut ctx,
            "memory.forget",
            json!({ "id": memory, "revision": revision }),
        )
        .await;
        assert_eq!(forgotten["memory"]["state"], "forgotten", "{forgotten}");

        // ── goal：新建 → 改下一步 → 等待 → 回到进行中 → 完成
        let added = ok(
            &tool,
            &mut ctx,
            "goal.add",
            json!({ "title": "空调巡检", "next": "看温度", "due": "2030-01-01" }),
        )
        .await;
        let goal = added["matter"]["id"].as_str().unwrap().to_string();
        assert_eq!(added["matter"]["state"], "active", "{added}");
        assert_eq!(added["matter"]["kind"], "goal", "{added}");
        let next = ok(
            &tool,
            &mut ctx,
            "goal.next",
            json!({ "id": goal, "next": "提工单" }),
        )
        .await;
        assert_eq!(next["matter"]["next"], "提工单", "{next}");
        let waiting = ok(
            &tool,
            &mut ctx,
            "goal.wait",
            json!({ "id": goal, "waiting_for": "SRE 审批", "revision": 2 }),
        )
        .await;
        assert_eq!(waiting["matter"]["state"], "waiting", "{waiting}");
        let resumed = ok(
            &tool,
            &mut ctx,
            "goal.resume",
            json!({ "id": goal, "next": "看审批意见" }),
        )
        .await;
        assert_eq!(resumed["matter"]["state"], "active", "{resumed}");
        // goal.list：只读动作不要求用户原话。
        let listed = ok(&tool, &mut ctx, "goal.list", json!({})).await;
        assert_eq!(listed["matters"][0]["id"], goal.as_str(), "{listed}");
        assert_eq!(listed["matters"].as_array().map(Vec::len), Some(1));

        // ── 定时是目标的配置：goal.schedule 设一个命令定时（不经模型），goal.list 带着它，
        //    goal.run 触发；goal.add 的 ongoing 建没有终点的目标，goal.retire 不再追踪它。
        let scheduled = ok(
            &tool,
            &mut ctx,
            "goal.schedule",
            json!({ "id": goal, "schedule": "0 9 * * *", "timezone": "UTC", "command": "true" }),
        )
        .await;
        assert_eq!(scheduled["schedule"]["job"]["id"], goal.as_str(), "{scheduled}");
        assert_eq!(scheduled["matter"]["revision"], resumed["matter"]["revision"], "定时不升版");
        let listed = ok(&tool, &mut ctx, "goal.list", json!({})).await;
        assert_eq!(listed["schedules"][0]["job"]["id"], goal.as_str(), "{listed}");
        let fired = ok(&tool, &mut ctx, "goal.run", json!({ "id": goal })).await;
        assert_eq!(fired["fired"]["job"], goal.as_str(), "{fired}");
        gateway
            .wait_terminal(&RunId::from_raw(fired["fired"]["run"].as_str().unwrap()))
            .await;
        let ongoing = ok(
            &tool,
            &mut ctx,
            "goal.add",
            json!({ "title": "每日巡检", "ongoing": true, "description": "看一眼告警",
                    "schedule": "0 9 * * *", "timezone": "UTC" }),
        )
        .await;
        assert_eq!(ongoing["matter"]["kind"], "area", "{ongoing}");
        assert_eq!(ongoing["schedule"]["job"]["prompt"], "看一眼告警", "{ongoing}");
        let ongoing_id = ongoing["matter"]["id"].as_str().unwrap().to_string();
        let listed = ok(&tool, &mut ctx, "goal.list", json!({})).await;
        assert_eq!(listed["matters"].as_array().map(Vec::len), Some(2), "没有终点的也列：{listed}");
        let retired = ok(&tool, &mut ctx, "goal.retire", json!({ "id": ongoing_id })).await;
        assert_eq!(retired["matter"]["state"], "retired", "{retired}");
        let unscheduled = ok(&tool, &mut ctx, "goal.unschedule", json!({ "id": goal })).await;
        assert!(unscheduled.get("schedule").is_none(), "{unscheduled}");

        let done = ok(&tool, &mut ctx, "goal.done", json!({ "id": goal })).await;
        assert_eq!(done["matter"]["state"], "done", "{done}");
        assert_eq!(
            done["matter"]["evidence"].as_array().map(Vec::len),
            Some(1),
            "完成引的是这个 Run 里用户的话：{done}"
        );

        // ── session / task
        let hits = ok(&tool, &mut ctx, "session.search", json!({ "q": "空调" })).await;
        assert_eq!(hits["hits"][0]["session"], session.as_str(), "{hits}");
        let sessions = ok(&tool, &mut ctx, "session.list", json!({})).await;
        assert!(
            sessions["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["session"] == session.as_str()),
            "{sessions}"
        );
        let tasks = ok(&tool, &mut ctx, "task.list", json!({ "all": true })).await;
        assert!(
            tasks["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["session"] == session.as_str()),
            "{tasks}"
        );
        gateway.running.stop().await;
    }

    /// 路由背后的规则工具照样继承：Run 里没有用户原话改不了承诺（`docs/goals.md` §4）。
    #[tokio::test]
    async fn the_handler_rules_apply_to_the_tool_a_run_without_user_words_cannot_commit() {
        let (_home, gateway) = start().await;
        let tool = komo_tool(&gateway);
        let (session, _run) = user_run(&gateway, "随便聊聊").await;
        let mut ctx = context(
            &session,
            &RunId::from_raw("not-a-run-of-this-session"),
            PlanSource::Interactive {
                session: session.clone(),
            },
        );
        let error = call(&tool, &mut ctx, "goal.add", json!({ "title": "发布观察" }))
            .await
            .unwrap_err();
        let ToolError::InvalidArguments { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("找不到用户说的话"), "{message}");
        gateway.running.stop().await;
    }

    /// 任务会话与 Cron 会话仍被现有规则拒绝：不能新建目标，也不能替用户记（`docs/goals.md` §4）。
    #[tokio::test]
    async fn a_task_session_and_a_cron_session_cannot_add_goals_through_the_tool() {
        let (_home, gateway) = start().await;
        let tool = komo_tool(&gateway);
        let (home_session, run) = user_run(&gateway, "开始做空调巡检").await;
        let core = gateway.state().core().unwrap().clone();
        let parent = core.session(home_session.as_str()).await.unwrap();

        // 任务会话：带着 `komo.task` 的子会话，哪怕 Run 里有"用户的话"也不算原话。
        let mut spec = NewSession::new("assistant")
            .kind(SessionKind::Task)
            .title("查空调");
        spec.parent_session = Some(parent.id.clone());
        let task = core.create_session(spec).await.unwrap();
        let doc = TaskDoc {
            parent: parent.root_conversation,
            anchor: TaskId::new(1),
            title: "查空调".into(),
            fork: false,
            model: None,
            effort: None,
            contracts: BTreeMap::new(),
            last_run: None,
            continued: None,
            stopped: false,
            goals: Vec::new(),
        };
        let root = task.root_conversation;
        core.engine
            .commit(move |tx| Box::pin(async move { store_task_doc(tx, root, &doc).await }))
            .await
            .unwrap();
        let task_session = SessionId::from_raw(task.id.clone());
        let mut ctx = context(
            &task_session,
            &run,
            PlanSource::Interactive {
                session: task_session.clone(),
            },
        );
        for (action, params) in [
            ("goal.add", json!({ "title": "替别人记的事" })),
            ("memory.add", json!({ "content": "任务正文不是用户原话" })),
        ] {
            let error = call(&tool, &mut ctx, action, params).await.unwrap_err();
            let ToolError::InvalidArguments { message } = &error else {
                panic!("{action}：{error:?}")
            };
            assert!(message.contains("后台任务"), "{action}：{message}");
        }

        // Cron 会话：Run 里没有用户说的话。
        let cron = core
            .create_session(
                NewSession::new("assistant")
                    .kind(SessionKind::Cron)
                    .title("每日简报"),
            )
            .await
            .unwrap();
        let cron_session = SessionId::from_raw(cron.id.clone());
        let mut ctx = context(
            &cron_session,
            &RunId::from_raw("cron-run"),
            PlanSource::Cron {
                job: komo_kernel::types::ids::CronJobId::from_raw("job-1"),
                job_version: 1,
            },
        );
        let error = call(&tool, &mut ctx, "goal.add", json!({ "title": "发布观察" }))
            .await
            .unwrap_err();
        let ToolError::InvalidArguments { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("找不到用户说的话"), "{message}");
        gateway.running.stop().await;
    }

    /// 造一个任务会话（带 `komo.task`），返回会话记录。
    async fn task_session(
        core: &crate::service::durable::DurableCore,
        parent: &komo_kernel::durable::SessionRecord,
        title: &str,
    ) -> komo_kernel::durable::SessionRecord {
        let mut spec = NewSession::new("assistant")
            .kind(SessionKind::Task)
            .title(title);
        spec.parent_session = Some(parent.id.clone());
        let task = core.create_session(spec).await.unwrap();
        let doc = TaskDoc {
            parent: parent.root_conversation,
            anchor: TaskId::new(1),
            title: title.into(),
            fork: false,
            model: None,
            effort: None,
            contracts: BTreeMap::new(),
            last_run: None,
            continued: None,
            stopped: false,
            goals: Vec::new(),
        };
        let root = task.root_conversation;
        core.engine
            .commit(move |tx| Box::pin(async move { store_task_doc(tx, root, &doc).await }))
            .await
            .unwrap();
        task
    }

    fn ctx_of(session: &SessionRecord, run: &RunId) -> ToolContext {
        let id = SessionId::from_raw(session.id.clone());
        context(
            &id,
            run,
            PlanSource::Interactive {
                session: id.clone(),
            },
        )
    }

    async fn new_matter(gateway: &Gw, kind: &str, title: &str) -> String {
        let mut body = json!({ "kind": kind, "title": title });
        if kind == "area" {
            body["standard"] = json!("标准");
        }
        let (status, body) = gateway.post_json("/v1/matters", body).await;
        assert_eq!(status, 200, "{body}");
        body["matter"]["id"].as_str().unwrap().to_string()
    }

    async fn task_rows(gateway: &Gw, query: &str) -> Value {
        let (status, body) = gateway.get_json(&format!("/v1/tasks{query}")).await;
        assert_eq!(status, 200, "{body}");
        body
    }

    /// 任务自己 `goal.attach` / `goal.detach`：清单行带着目标的 id 与标题；重复挂幂等；挂不存在的 id、挂领域被拒；
    /// 没有用户原话的一轮（监听事件起的）照样能挂，因为改的是任务自己的关联、不是承诺。
    #[tokio::test]
    async fn a_task_attaches_and_detaches_goals_that_show_in_the_task_list() {
        let (_home, gateway) = start().await;
        let tool = komo_tool(&gateway);
        let (home_session, run) = user_run(&gateway, "开始做空调巡检").await;
        let core = gateway.state().core().unwrap().clone();
        let parent = core.session(home_session.as_str()).await.unwrap();
        let task = task_session(&core, &parent, "查空调").await;
        let goal = new_matter(&gateway, "goal", "空调巡检").await;
        let area = new_matter(&gateway, "area", "家里的设备").await;

        let mut ctx = ctx_of(&task, &run);
        let attached = ok(&tool, &mut ctx, "goal.attach", json!({ "id": goal })).await;
        assert_eq!(attached["changed"], true, "{attached}");
        assert_eq!(attached["goal"]["title"], "空调巡检", "{attached}");
        let again = ok(&tool, &mut ctx, "goal.attach", json!({ "id": goal })).await;
        assert_eq!(again["changed"], false, "重复挂无变化：{again}");

        let rows = task_rows(&gateway, "").await;
        let row = rows["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["session"] == task.id.as_str())
            .unwrap_or_else(|| panic!("{rows}"));
        assert_eq!(
            row["goals"],
            json!([{ "id": goal, "title": "空调巡检" }]),
            "只挂了一次：{row}"
        );

        for (id, expect) in [("no-such-goal", "no-such-goal"), (area.as_str(), "领域")] {
            let error = call(&tool, &mut ctx, "goal.attach", json!({ "id": id }))
                .await
                .unwrap_err();
            let ToolError::InvalidArguments { message } = &error else {
                panic!("{id}：{error:?}")
            };
            assert!(message.contains(expect), "{id}：{message}");
        }

        // 没有用户原话的一轮：goal.add 被拒，goal.attach 仍可用。
        let mut quiet = ctx_of(&task, &RunId::from_raw("not-a-run-of-this-session"));
        let error = call(&tool, &mut quiet, "goal.add", json!({ "title": "新的" }))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );
        let other = new_matter(&gateway, "goal", "换滤网").await;
        let attached = ok(&tool, &mut quiet, "goal.attach", json!({ "id": other })).await;
        assert_eq!(attached["changed"], true, "{attached}");

        // detach：摘掉一个；没挂过的也成功。
        let detached = ok(&tool, &mut ctx, "goal.detach", json!({ "id": goal })).await;
        assert_eq!(detached["changed"], true, "{detached}");
        let never = ok(
            &tool,
            &mut ctx,
            "goal.detach",
            json!({ "id": "never-attached" }),
        )
        .await;
        assert_eq!(never["changed"], false, "{never}");
        let rows = task_rows(&gateway, "").await;
        let row = rows["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["session"] == task.id.as_str())
            .unwrap();
        assert_eq!(
            row["goals"],
            json!([{ "id": other, "title": "换滤网" }]),
            "{row}"
        );
        gateway.running.stop().await;
    }

    /// 非任务会话（主会话）里调用被拒，说明只有任务里能用。
    #[tokio::test]
    async fn goal_attach_is_refused_outside_a_task() {
        let (_home, gateway) = start().await;
        let tool = komo_tool(&gateway);
        let (home_session, run) = user_run(&gateway, "开始做空调巡检").await;
        let core = gateway.state().core().unwrap().clone();
        let parent = core.session(home_session.as_str()).await.unwrap();
        let goal = new_matter(&gateway, "goal", "空调巡检").await;

        let mut ctx = ctx_of(&parent, &run);
        for action in ["goal.attach", "goal.detach"] {
            let error = call(&tool, &mut ctx, action, json!({ "id": goal }))
                .await
                .unwrap_err();
            let ToolError::InvalidArguments { message } = &error else {
                panic!("{action}：{error:?}")
            };
            assert!(message.contains("只有任务里能用"), "{action}：{message}");
        }

        gateway.running.stop().await;
    }

    /// `GET /v1/tasks?goal=` 与 `task.list` 的 `goal` 只返回带着这个键的任务，分页照常；`goal.list` 带计数；
    /// 目标完成后任务上的键还在。
    #[tokio::test]
    async fn the_goal_filter_counts_and_closing_a_goal_keep_the_keys() {
        let (_home, gateway) = start().await;
        let tool = komo_tool(&gateway);
        let (home_session, run) = user_run(&gateway, "开始做空调巡检").await;
        let core = gateway.state().core().unwrap().clone();
        let parent = core.session(home_session.as_str()).await.unwrap();
        let goal = new_matter(&gateway, "goal", "空调巡检").await;
        let other = new_matter(&gateway, "goal", "换滤网").await;

        let mut mine = Vec::new();
        for title in ["一", "二", "三"] {
            let task = task_session(&core, &parent, title).await;
            ok(
                &tool,
                &mut ctx_of(&task, &run),
                "goal.attach",
                json!({ "id": goal }),
            )
            .await;
            mine.push(task.id.clone());
        }
        let stray = task_session(&core, &parent, "无关").await;
        let filter = task_session(&core, &parent, "别的目标").await;
        ok(
            &tool,
            &mut ctx_of(&filter, &run),
            "goal.attach",
            json!({ "id": other }),
        )
        .await;

        // HTTP：只返回带着 goal 的三个，分页走完不重复不漏。
        let all = task_rows(&gateway, &format!("?goal={goal}")).await;
        let mut ids: Vec<String> = all["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["session"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        let mut expect = mine.clone();
        expect.sort();
        assert_eq!(ids, expect, "{all}");
        assert!(!ids.contains(&stray.id));
        let mut walked = Vec::new();
        let mut cursor = String::new();
        loop {
            let page = task_rows(&gateway, &format!("?goal={goal}&limit=2{cursor}")).await;
            let rows = page["tasks"].as_array().unwrap();
            assert!(rows.len() <= 2, "{page}");
            walked.extend(
                rows.iter()
                    .map(|row| row["session"].as_str().unwrap().to_string()),
            );
            match page["next_cursor"].as_str() {
                Some(next) => cursor = format!("&cursor={next}"),
                None => break,
            }
        }
        walked.sort();
        assert_eq!(walked, expect, "分页逐页走完");

        // 工具：同样的过滤。
        let mut ctx = context(
            &home_session,
            &run,
            PlanSource::Interactive {
                session: home_session.clone(),
            },
        );
        let listed = ok(&tool, &mut ctx, "task.list", json!({ "goal": other })).await;
        let rows = listed["tasks"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{listed}");
        assert_eq!(rows[0]["session"], filter.id.as_str(), "{listed}");

        // goal.list 带计数。
        let goals = ok(&tool, &mut ctx, "goal.list", json!({})).await;
        let count = |id: &str| {
            goals["matters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|matter| matter["id"] == id)
                .unwrap_or_else(|| panic!("{goals}"))["tasks"]
                .clone()
        };
        assert_eq!(
            (count(&goal), count(&other)),
            (json!(3), json!(1)),
            "{goals}"
        );

        // 目标完成：任务上的键原样留着（不级联清理），清单与过滤照旧。
        let done = ok(&tool, &mut ctx, "goal.done", json!({ "id": goal })).await;
        assert_eq!(done["matter"]["state"], "done", "{done}");
        let after = task_rows(&gateway, &format!("?goal={goal}")).await;
        assert_eq!(after["tasks"].as_array().unwrap().len(), 3, "{after}");
        assert_eq!(
            after["tasks"][0]["goals"][0]["title"], "空调巡检",
            "{after}"
        );
        let goals = ok(&tool, &mut ctx, "goal.list", json!({ "all": true })).await;
        let closed = goals["matters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|matter| matter["id"] == goal.as_str())
            .unwrap();
        assert_eq!(closed["tasks"], 3, "{goals}");
        gateway.running.stop().await;
    }

    /// 查记忆也过会话的闸：`memory = off` 的 Agent 查不了（与 `GET /v1/memories?session=` 同一处判断）。
    #[tokio::test]
    async fn a_memory_search_is_gated_by_the_session_like_the_route() {
        let config = format!("{CONFIG}\n[agents.quiet]\nmemory = \"off\"\n")
            .replace("[agents.assistant]\n", "[agents.assistant]\n\n");
        let home = Home::with_config(&config);
        let gateway = home.start(FakeLlm::finisher("好")).await;
        let tool = komo_tool(&gateway);
        let core = gateway.state().core().unwrap().clone();
        let quiet = core.main_session("quiet").await.unwrap().0;
        let session = SessionId::from_raw(quiet.id);
        let mut ctx = context(
            &session,
            &RunId::from_raw("any"),
            PlanSource::Interactive {
                session: session.clone(),
            },
        );
        let error = call(&tool, &mut ctx, "memory.search", json!({ "query": "空调" }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("memory = off"), "{error}");
        gateway.running.stop().await;
    }

    /// 端到端：模型在一个真 Run 里调 `komo`——系统提示指向这个工具（没有旧的 shell 说法）、工具
    /// 声明里有 action enum 而 `params` 的字段不进 schema；调用经执行器与真处理函数落账，模型收到
    /// 的结果里 `memory.search` 带宿主标记，`params` 填错得到列出字段的报错。
    #[tokio::test]
    async fn a_run_calls_the_tool_and_sees_the_prompt_the_schema_and_the_results() {
        use crate::service::test_support::harness::{call_round, text_round};
        use komo_kernel::types::turn::RoundInput;

        let home = Home::with_config(CONFIG);
        let llm = FakeLlm::new(vec![vec![
            call_round(
                1,
                "c1",
                "komo",
                json!({ "action": "memory.search", "params": { "query": "空调", "mode": "keyword" } }),
            ),
            call_round(
                2,
                "c2",
                "komo",
                json!({ "action": "memory.add", "params": { "text": "x" } }),
            ),
            text_round(3, "好的。"),
        ]]);
        let gateway = home.start(Arc::clone(&llm)).await;
        let session = gateway.open_session().await;
        let run = gateway
            .submit(&session, "komo-e2e", "查一下空调的记忆")
            .await
            .run;
        gateway.wait_terminal(&run).await;

        let requests = llm.requests.lock().unwrap().clone();
        let first = &requests[0];
        assert!(
            first.system_prompt.contains("tool://komo/doc"),
            "{}",
            first.system_prompt
        );
        assert!(
            !first.system_prompt.contains("komo memory"),
            "{}",
            first.system_prompt
        );
        let declared = first
            .tools
            .iter()
            .find(|tool| tool.name == "komo")
            .expect("komo 声明给了模型");
        let actions = declared.parameters["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        assert!(actions.contains(&json!("goal.link")) && actions.contains(&json!("task.list")));
        assert!(
            declared.parameters["properties"]["params"]
                .get("properties")
                .is_none()
        );

        let inputs = llm.inputs.lock().unwrap().clone();
        let results: Vec<_> = inputs
            .iter()
            .filter_map(|input| match input {
                RoundInput::ToolResults { results, .. } => Some(results.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(!results[0].is_error, "{}", results[0].content);
        assert!(
            results[0]
                .content
                .contains(komo_kernel::types::memory::MEMORY_QUERY_MARKER),
            "{}",
            results[0].content
        );
        assert!(results[1].is_error, "{}", results[1].content);
        assert!(
            results[1].content.contains("不认识的字段 `text`")
                && results[1].content.contains("content（必填）"),
            "{}",
            results[1].content
        );
        gateway.running.stop().await;
    }

    // ------------------------------------------------------------ intervention.*

    /// 一台带 `demo` MCP 服务器的 Gateway，`bump` 声明为对外（无人值守的 Run 调它要先确认；每执行一次往计数
    /// 文件写一行）。
    fn home_with_outward() -> (Home, std::path::PathBuf) {
        let home = Home::new();
        let server = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp/server.py");
        let counter = home.path().join("bump.count");
        let config = format!(
            "{}\n[mcp.servers.demo]\ncommand = \"python3\"\nargs = [\"{}\", \"{}\"]\nexposure = \"direct\"\noutward = [\"bump\"]\n",
            crate::service::test_support::harness::home_config(),
            server.display(),
            counter.display(),
        );
        std::fs::write(home.path().join("config.toml"), config).unwrap();
        (home, counter)
    }

    /// 触发器（定时）起的一条 Run：往 `session` 的当前分支放一条 `cron.input` 输入。
    async fn trigger_run(gateway: &Gw, session: &SessionId) -> RunId {
        use komo_runtime::durable::harness::{InputEntry, SubmissionDraft};
        let core = gateway.state().core().unwrap();
        let record = core.session(session.as_str()).await.unwrap();
        let id = core
            .harness
            .submit(
                &core.engine,
                record.current_branch,
                SubmissionDraft::input("每日巡检").with_entry(InputEntry::new("cron.input", None)),
            )
            .await
            .unwrap();
        komo_kernel::durable::legacy_ids::run_id(id)
    }

    /// 计数文件里有几行 = 副作用发生了几次。
    fn lines(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
            .unwrap_or(0)
    }

    fn denied(result: Result<ToolOutput, ToolError>) -> String {
        match result {
            Err(ToolError::Denied { reason }) => reason,
            other => panic!("应当被拒绝：{other:?}"),
        }
    }

    /// 操作者亲自发消息开始的 Run 里能答复；触发器起的 Run 里一律拒绝；别的对话里的、任务会话里的也不行；
    /// confirm 不认的答复报错；答过的返回原结论。
    #[tokio::test]
    async fn intervention_answer_works_in_an_operator_run_and_is_refused_in_a_trigger_run() {
        use komo_kernel::protocol::http::InterventionKind;
        let (home, counter) = home_with_outward();
        let llm = FakeLlm::new(vec![
            vec![crate::service::test_support::harness::text_round(1, "好")],
            vec![
                crate::service::test_support::harness::call_round(
                    1,
                    "pc-1",
                    "mcp__demo__bump",
                    json!({}),
                ),
                crate::service::test_support::harness::text_round(2, "加好了"),
            ],
        ]);
        let gateway = home.start(llm).await;
        let tool = komo_tool(&gateway);
        let core = gateway.state().core().unwrap().clone();

        // 支线对话：操作者说了一句话（Run 1），之后一条触发器起的 Run 调对外工具、停在确认上。
        let (chat, operator_run) = user_run(&gateway, "每天早上帮我巡检一下").await;
        let trigger = trigger_run(&gateway, &chat).await;
        let pending = gateway.wait_intervention(&trigger).await;
        assert_eq!(pending.kind, InterventionKind::Confirm);
        let handle = pending.handle.clone();
        let (other, other_run) = user_run(&gateway, "另一段对话").await;
        let source = |session: &SessionId| PlanSource::Interactive {
            session: session.clone(),
        };

        // 清单：只列指向当前对话的。
        let mut ctx = context(&chat, &operator_run, source(&chat));
        let listed = ok(&tool, &mut ctx, "intervention.list", json!({})).await;
        assert_eq!(
            listed["interventions"][0]["handle"],
            handle.as_str(),
            "{listed}"
        );
        let mut elsewhere = context(&other, &other_run, source(&other));
        let listed = ok(&tool, &mut elsewhere, "intervention.list", json!({})).await;
        assert!(
            listed["interventions"].as_array().unwrap().is_empty(),
            "{listed}"
        );

        // 触发器起的 Run 里答复：拒绝。
        let mut in_trigger = context(&chat, &trigger, source(&chat));
        let reason = denied(
            call(
                &tool,
                &mut in_trigger,
                "intervention.answer",
                json!({ "handle": handle, "verdict": "approve" }),
            )
            .await,
        );
        assert!(reason.contains("不是操作者发消息开始的"), "{reason}");
        assert_eq!(lines(&counter), 0);
        // 任务会话里答复：拒绝。
        let mut spec = NewSession::new("assistant").kind(komo_kernel::durable::SessionKind::Task);
        spec.parent_session = Some(chat.as_str().to_owned());
        let task = SessionId::from_raw(core.create_session(spec).await.unwrap().id);
        let mut in_task = context(&task, &operator_run, source(&task));
        denied(
            call(
                &tool,
                &mut in_task,
                "intervention.answer",
                json!({ "handle": handle, "verdict": "approve" }),
            )
            .await,
        );
        // 别的对话里的操作者 Run 不能答这段对话的事。
        let mut wrong_chat = context(&other, &other_run, source(&other));
        let error = call(
            &tool,
            &mut wrong_chat,
            "intervention.answer",
            json!({ "handle": handle, "verdict": "approve" }),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("不是在这段对话里等你答复的"),
            "{error}"
        );

        // 操作者的 Run 里：confirm 不认的答复报错并列出能答的。
        let error = call(
            &tool,
            &mut ctx,
            "intervention.answer",
            json!({ "handle": handle, "verdict": "abandon" }),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("approve"), "{error}");
        assert_eq!(lines(&counter), 0, "答之前不执行");
        // 认的答复：提交，之后那条触发器 Run 照常执行。
        let answered = ok(
            &tool,
            &mut ctx,
            "intervention.answer",
            json!({ "handle": handle, "verdict": "approve" }),
        )
        .await;
        assert_eq!(answered["verdict"], "approve");
        assert_eq!(answered["already_answered"], false);
        gateway.wait_terminal(&trigger).await;
        assert_eq!(lines(&counter), 1, "批准后只执行一次");
        gateway.running.stop().await;
    }

    /// 出错的任务（blocked，只认 abandon）在对话里答复；答过的再答返回原结论，认不得的结论报错。
    #[tokio::test]
    async fn a_faulted_run_is_answered_from_its_chat_and_a_second_answer_returns_the_first() {
        let (_home, gateway) = start().await;
        let tool = komo_tool(&gateway);
        let core = gateway.state().core().unwrap().clone();
        let (chat, run) = user_run(&gateway, "在吗").await;
        let branch = core.session(chat.as_str()).await.unwrap().current_branch;
        core.engine
            .commit(move |tx| {
                Box::pin(async move {
                    use crate::service::durable::sessions::agent_doc::{
                        AGENT_DOC_VERSION, address,
                    };
                    tx.put_document(
                        address(branch),
                        AGENT_DOC_VERSION,
                        json!("坏掉的 komo.agent"),
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let stalled = gateway.submit(&chat, "komo-2", "跑一下").await.run;
        let handle = gateway.wait_intervention(&stalled).await.handle;
        let mut ctx = context(
            &chat,
            &run,
            PlanSource::Interactive {
                session: chat.clone(),
            },
        );

        let error = call(
            &tool,
            &mut ctx,
            "intervention.answer",
            json!({ "handle": handle, "verdict": "approve" }),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("abandon"), "{error}");
        let first = ok(
            &tool,
            &mut ctx,
            "intervention.answer",
            json!({ "handle": handle, "verdict": "abandon" }),
        )
        .await;
        assert_eq!(first["already_answered"], false, "{first}");
        let second = ok(
            &tool,
            &mut ctx,
            "intervention.answer",
            json!({ "handle": handle, "verdict": "abandon" }),
        )
        .await;
        assert_eq!(second["already_answered"], true, "{second}");
        let listed = ok(&tool, &mut ctx, "intervention.list", json!({})).await;
        assert!(
            listed["interventions"].as_array().unwrap().is_empty(),
            "{listed}"
        );
        gateway.running.stop().await;
    }
}
