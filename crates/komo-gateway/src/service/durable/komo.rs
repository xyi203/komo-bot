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
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::tool::ToolError;
use komo_tools::tools::komo::{KomoCall, KomoService};
use serde::Serialize;
use serde_json::Value;

use crate::http::error::{ApiFailure, ApiResult};
use crate::http::idempotency::Idempotency;
use crate::http::v2::{sessions, tasks};
use crate::http::{Api, cron, matters, memories};
use crate::service::durable::sessions::is_group_task;
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
        // 群的线派出的任务一个动作都不能用（逐个动作的理由见 [`group_task_refusal`]）。
        if is_group_task(&*api.state.durable.storage, caller.as_str()).await {
            return Err(group_task_refusal(&call));
        }
        match call {
            KomoCall::MemoryAdd(request) => reply(memories::add(State(api), Json(request)).await),
            KomoCall::MemorySearch(query) => reply(memories::list(State(api), Query(query)).await),
            KomoCall::MemoryForget { id, request } => {
                reply(memories::forget(State(api), Path(id.to_string()), Json(request)).await)
            }
            KomoCall::GoalAdd(request) => reply(matters::add(State(api), Json(request)).await),
            KomoCall::GoalList(query) => reply(matters::list(State(api), Query(query)).await),
            KomoCall::GoalAct { id, verb, request } => reply(
                matters::act(
                    State(api),
                    Path((id.to_string(), verb.as_str().to_string())),
                    Json(request),
                )
                .await,
            ),
            KomoCall::CronList => reply(cron::list(State(api)).await),
            KomoCall::CronRun { job, request } => {
                reply(cron::run_now(State(api), Path(job.to_string()), Json(request)).await)
            }
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
            KomoCall::TaskList(query) => reply(
                tasks::list(
                    State(api),
                    list_uri(query.all),
                    Query(sessions::ListPaging {
                        limit: query.limit,
                        cursor: query.cursor,
                    }),
                )
                .await,
            ),
        }
    }
}

/// 群的线派出的任务（[`is_group_task`]）调 `komo` 的任何动作都拒绝，按动作分两句话：
///
/// - `memory.*` / `goal.*`：操作者的个人记忆与目标，读和写都不行；
/// - `cron.list` / `cron.run`：定时任务的清单与触发——任务说明里是操作者自己写的东西，触发它是以操作者的身份
///   起一次 Run；`session.search` / `session.list`：搜索与列出会话——正好是操作者私聊里的内容；
///   `task.list`：任务清单是全部任务（含私聊派出的）的标题与状态。这些都会把操作者私聊里的东西带到群里，
///   所以也拒绝。（`task.list` 没有"只列这个群"的参数，不能放行一半。）
fn group_task_refusal(call: &KomoCall) -> ToolError {
    let reason = match call {
        KomoCall::MemoryAdd(_)
        | KomoCall::MemorySearch(_)
        | KomoCall::MemoryForget { .. }
        | KomoCall::GoalAdd(_)
        | KomoCall::GoalList(_)
        | KomoCall::GoalAct { .. } => "群里派出的任务不能读写个人记忆与目标",
        KomoCall::CronList
        | KomoCall::CronRun { .. }
        | KomoCall::SessionSearch(_)
        | KomoCall::SessionList(_)
        | KomoCall::TaskList(_) => {
            "群里派出的任务不能查看或触发定时任务、会话与任务清单（里面有操作者私聊里的内容）"
        }
    };
    ToolError::Denied {
        reason: reason.to_owned(),
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

    /// 十五个动作各走通一次，经由真的处理函数（真 aux.db / durable.db，不是桩）：返回的是各自
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
        // goal.list：只列目标；只读动作不要求用户原话。
        let listed = ok(&tool, &mut ctx, "goal.list", json!({})).await;
        assert_eq!(listed["matters"][0]["id"], goal.as_str(), "{listed}");
        assert_eq!(listed["matters"].as_array().map(Vec::len), Some(1));

        // ── cron：建一个命令 Job（不经模型），goal.link 挂上，cron.list 看得到，cron.run 触发
        let (status, job) = gateway
            .post_json(
                "/v1/cron",
                json!({ "name": "每日巡检", "schedule": "0 9 * * *", "timezone": "UTC",
                        "prompt": "", "command": "true" }),
            )
            .await;
        assert_eq!(status, 200, "{job}");
        let job = job["id"].as_str().unwrap().to_string();
        let linked = ok(
            &tool,
            &mut ctx,
            "goal.link",
            json!({ "id": goal, "cron": job }),
        )
        .await;
        assert_eq!(
            linked["matter"]["triggers"][0]["job"],
            job.as_str(),
            "{linked}"
        );
        let jobs = ok(&tool, &mut ctx, "cron.list", json!({})).await;
        assert_eq!(jobs["jobs"][0]["id"], job.as_str(), "{jobs}");
        let fired = ok(&tool, &mut ctx, "cron.run", json!({ "id": job })).await;
        assert_eq!(fired["job"], job.as_str(), "{fired}");
        gateway
            .wait_terminal(&RunId::from_raw(fired["run"].as_str().unwrap()))
            .await;

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
            reply: None,
            stopped: false,
            thread: None,
            session: None,
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
}
