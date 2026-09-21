//! 资源命名空间（`docs/komo_bot.md` §六、§4.7）：`skill://` / `tool://` / `artifact://`
//! 三种**只读**入口的端到端验收。
//!
//! 五条，每一条盯着一件"模型看到的东西":
//!
//! - **本地路径照旧**：`read` 一个 workspace 里的真文件，正文回来，计划里还是普通路径——
//!   兼容性就是"不出现资源语义"；
//! - **`skill://`**：数据目录里一份真 skill 读到正文，且审计里的计划目标是**逻辑 URI**
//!   （审批 / 审计回答的是"允许读哪个逻辑资源"，不是它落在哪个文件）；
//! - **`tool://`**：能力面之外的工具**在 `prepare` 就拒绝**（不是 Ask），面内的读到
//!   JSON Schema；
//! - **`artifact://`**：python 写进 `$KOMO_ARTIFACT_DIR` 的文件登记成这次调用的产物，
//!   **重启 Gateway 之后**按同一条 URI 读到的正文与第一次一致；
//! - **别的会话的 run**：拿另一个会话的 `run` 拼出来的 URI 读不到——那是会话边界。
//!
//! 共用件在 `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、
//! 脚本化模型）。这里断言的是**模型收到了什么**（`FakeLlm` 记下的轮输入）与**账本里写了
//! 什么**（`tool.planned` 的计划、`output.json` 的产物引用）。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, Home, call_round, text_round, write_home_with,
};
use komo_kernel::events::{Event, EventPayload, ToolResult};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::refs::{ContentRef, ToolResultBody, ToolResultStatus};
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::{LlmError, Round, RoundInput};

// ---------------------------------------------------------------- 配置

/// 一份最小的 config.toml，`agents` 是 `[agents.assistant]` 那一节。
///
/// **自己写而不是拼 `config_toml`**：这几条要按 Agent 收工具面（`tools = […]`），而
/// `config_toml` 已经写了 `[agents.assistant]`，再写一次就是 TOML 的重复表。
fn config(agents: &str) -> String {
    format!(
        r#"
default_agent = "assistant"

{agents}

[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 111

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
"#
    )
}

/// 一份给 Agent 的工具面。
fn tools(list: &str) -> String {
    format!("[agents.assistant]\ntools = [{list}]\n")
}

/// 一个数据目录 + 一份给定的 config.toml。
///
/// 与 `tests/agents` 的 `rewrite_config` 同一个做法：几条 config 要引用数据目录**自己**的
/// 路径（skills 根），而 `Home::with_config` 建临时目录时那个路径还不知道。
fn home_with(config: &str) -> Home {
    let home = Home::new();
    write_home_with(home.path(), config, DEFAULT_ENV, None);
    home
}

/// 这台机器上有没有系统解释器——`python` 工具要有它才挂得起来（`service::python_env`
/// 在受管理环境没建出来时退回 `python3`）。
fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

// ---------------------------------------------------------------- 轮脚本

/// 一次 `read`（路径可以是本地路径，也可以是资源 URI）。
fn read_call(n: u32, id: &str, path: &str) -> Result<Round, LlmError> {
    call_round(n, id, "read", serde_json::json!({ "path": path }))
}

/// 一次 `python` code 调用：往 `$KOMO_ARTIFACT_DIR` 写一个文件（§4.7 的产出目录约定）。
fn python_writes(round: u32, name: &str, body: &str) -> Result<Round, LlmError> {
    call_round(
        round,
        "pc-python",
        "python",
        serde_json::json!({
            "mode": "code",
            "code": format!(
                "import os, pathlib\n\
                 target = pathlib.Path(os.environ['KOMO_ARTIFACT_DIR'], {name:?})\n\
                 target.write_text({body:?}, encoding='utf-8')\n\
                 result = 'ok'"
            ),
        }),
    )
}

// ---------------------------------------------------------------- 断言助手

/// 模型被喂回来的**全部**工具结果，按顺序：正文 + 是不是错误。
///
/// 一次运行里同一个工具可以调多次（`tests/agents` 的 `fed_back` 只按 `needle` 取第一条），
/// 而这里要按顺序看每一条——被 `prepare` 拒绝的调用交回模型的是执行器那句话本身，不是
/// 投影出来的 `[工具 · 状态 · 耗时]` 抬头，所以不能只按抬头找。
fn fed_back(llm: &FakeLlm) -> Vec<(String, bool)> {
    llm.inputs
        .lock()
        .expect("轮输入")
        .iter()
        .flat_map(|input| match input {
            RoundInput::ToolResults { results } => results
                .iter()
                .map(|result| (result.content.clone(), result.is_error))
                .collect::<Vec<_>>(),
            RoundInput::First => Vec::new(),
        })
        .collect()
}

/// 这个 Session 里每一份 `tool.planned` 的计划（按顺序）。
///
/// 审计里那条就是"允许读哪个逻辑资源"的答案：资源目标印的是**逻辑 URI**。
fn planned(events: &[Event]) -> Vec<ExecutionPlan> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolPlanned(planned) => planned.plan.as_deref().cloned(),
            _ => None,
        })
        .collect()
}

/// 这个 Session 里第一条 `tool.result` 的**完整正文**（`output.json` 里的 `body`，§8.3）。
///
/// 事件里那条只有状态、`output_ref` 与 ≤1 KiB 预览；`output.json` 本身是一层外壳
/// （身份 + `status` + `body`），这次的产物引用在 `body.artifacts` 里。
fn body_of(home: &Home, session: &SessionId) -> ToolResultBody {
    let events = home.events(session);
    let result: &ToolResult = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("有一条 tool.result");
    let path = home.session_dir(session).join(result.output_ref.path());
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读 {} 失败：{error}", path.display()));
    serde_json::from_str::<komo_store::tool_output::OutputDocument>(&raw)
        .expect("output.json 解析得出")
        .body
}

/// 一条产物引用 → 读它的 URI。
///
/// 就是 §4.7 那句：`ContentRef.path` 相对会话目录（`artifacts/<run>/<…>`），正文里给出
/// `artifact://files/<run>/<名字>`。
fn uri_of(artifact: &ContentRef) -> String {
    let relative = artifact
        .path
        .strip_prefix("artifacts/")
        .unwrap_or_else(|| panic!("产物路径该以 artifacts/ 开头：{}", artifact.path));
    format!("artifact://files/{relative}")
}

/// 工具结果投影正文里那**一行产物入口**（`产物：artifact://files/<run>/<名字>（13 B）`）
/// 里给出的 URI。
///
/// 模型手里只有这一行——测试就用它去 `read`，这样验的正是"模型看到的那一条到底读不读得
/// 回来"，而不是测试自己另拼一条同义的 URI。
fn produced_uri(fed: &str) -> String {
    let marker = "产物：";
    let start = fed
        .find(marker)
        .unwrap_or_else(|| panic!("正文里没有产物那一行：{fed}"))
        + marker.len();
    let rest = &fed[start..];
    let end = rest
        .find('（')
        .unwrap_or_else(|| panic!("产物入口后面该跟着大小：{rest}"));
    rest[..end].to_string()
}

// ---------------------------------------------------------------- 本地路径照旧

/// `read` 一个 workspace 里的真文件：正文回来，而且计划里**还是普通路径**——资源那一维
/// 没有把旧用法改掉（序列化上 `TargetRef` 那条与改造前逐字相同）。
#[tokio::test]
async fn a_local_path_reads_as_plain_content_without_resource_semantics() {
    let home = home_with(&config(&tools("\"read\", \"rg\"")));
    let file = home.workspace().join("笔记.txt");
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    std::fs::write(&file, "本地文件的内容\n").expect("写文件");

    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-read", "笔记.txt"),
        text_round(2, "读完了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "res-local", "读那份笔记").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].1, "普通文件读得到：{}", results[0].0);
    assert!(
        results[0].0.contains("本地文件的内容"),
        "正文就是那份文件：{}",
        results[0].0
    );

    let plan = planned(&home.events(&session))
        .into_iter()
        .next()
        .expect("有 tool.planned");
    assert!(plan.targets[0].uri().is_none(), "本地路径没有资源语义");
    let described = plan.targets[0].describe();
    assert!(
        described.ends_with("笔记.txt") && !described.contains("://"),
        "审批/审计里印的就是路径本身：{described}"
    );
}

// ---------------------------------------------------------------- skill://

/// 数据目录里一份真 skill：`read("skill://review/SKILL.md")` 读到正文，且审计里的计划
/// 目标是**逻辑 URI**（`skill://review/SKILL.md（真实路径）`）——不产生审批。
#[tokio::test]
async fn a_skill_uri_reads_and_the_plan_names_the_logical_uri() {
    let home = Home::new();
    let skill_dir = home.path().join("skills");
    let skill = skill_dir.join("review").join("SKILL.md");
    std::fs::create_dir_all(skill.parent().expect("skill 目录")).expect("建 skill 目录");
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: 怎么审一个 PR\n---\n# 审查步骤\n先看 diff。\n",
    )
    .expect("写 SKILL.md");
    write_home_with(
        home.path(),
        &config(&format!(
            "[paths]\nskill_dirs = [\"{}\"]\n\n{}",
            skill_dir.display(),
            tools("\"read\", \"rg\"")
        )),
        DEFAULT_ENV,
        None,
    );

    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-skill", "skill://review/SKILL.md"),
        text_round(2, "看过了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;
    let run = gw
        .submit(&session, "res-skill", "按审查 skill 看一遍")
        .await
        .run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        !results[0].1,
        "skill 是只读根，直接读得到：{}",
        results[0].0
    );
    assert!(
        results[0].0.contains("先看 diff"),
        "拿到的是那份 SKILL.md 的正文：{}",
        results[0].0
    );

    let plan = planned(&home.events(&session))
        .into_iter()
        .next()
        .expect("有 tool.planned");
    let target = &plan.targets[0];
    assert_eq!(
        target.uri().map(ToString::to_string),
        Some("skill://review/SKILL.md".to_string()),
        "计划里带的是**逻辑** URI"
    );
    let described = target.describe();
    assert!(
        described.starts_with("skill://review/SKILL.md（"),
        "审批/审计那一行印逻辑 URI，真实路径跟在后面：{described}"
    );
    assert!(
        described.ends_with("SKILL.md）"),
        "真实路径也要看得见：{described}"
    );

    gw.stop().await;
}

// ---------------------------------------------------------------- tool://

/// `tool://` 是**虚拟入口**：面内的读到 JSON Schema；面外的（`write` 不在这次能力面上）
/// **在 `prepare` 就拒绝**——不是 Ask，也不是先试一下再说。
#[tokio::test]
async fn tool_uris_read_schema_inside_the_surface_and_are_refused_outside_it() {
    let home = home_with(&config(&tools("\"read\", \"rg\"")));
    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-in", "tool://read/schema"),
        read_call(2, "pc-out", "tool://write/schema"),
        text_round(3, "看完了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "res-tool", "看看工具说明").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm);
    assert_eq!(results.len(), 2, "两次读，两条结果：{results:?}");

    // 面内：真的给了 read 的 JSON Schema。
    assert!(!results[0].1, "面内的工具读得到：{}", results[0].0);
    assert!(
        results[0].0.contains("\"properties\"") && results[0].0.contains("\"path\""),
        "面内读到的是 JSON Schema：{}",
        results[0].0
    );

    // 面外：拒绝，而且是一句说清原因的话——不进计划就没有执行（**不是** Ask）。
    assert!(results[1].1, "面外的工具是错误结果：{}", results[1].0);
    assert!(
        !results[1].0.contains("\"properties\""),
        "被拒绝的调用什么都没给回来：{}",
        results[1].0
    );
    assert!(
        results[1].0.contains("不在这次能力面里") && results[1].0.contains("write"),
        "那句话要说清是哪个工具、这次能用哪些：{}",
        results[1].0
    );

    gw.stop().await;
}

// ---------------------------------------------------------------- artifact://

/// python 写进 `$KOMO_ARTIFACT_DIR` 的文件被登记成这次调用的产物（`ContentRef`），
/// 之后可用 `artifact://files/<run>/<名字>` 读回；**重启 Gateway**（同一个数据目录）之后
/// 读到的正文与第一次一致——产物是落盘的文件，不是内存里的一份。
#[tokio::test]
async fn python_artifacts_are_registered_and_readable_after_a_restart() {
    if !have_python() {
        return;
    }
    let home = home_with(&config(&tools("\"read\", \"rg\", \"python\"")));
    let llm = FakeLlm::new(vec![
        // Run A：调一次 python（任意 code 默认停审批，§7.1 第 5 行）。
        vec![python_writes(1, "报告.md", "产物正文\n")],
        // 批准之后的续跑段：说一句话收尾。
        vec![text_round(2, "写好了。")],
    ]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;

    let run_a = gw
        .submit(&session, "res-art", "把报告写到产物目录")
        .await
        .run;
    let approval = gw.wait_approval().await;
    gw.decide(&approval.approval, true).await;
    let detail = gw.wait_terminal(&run_a).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    // 账本里那条 `tool.result` 的完整正文带着这次的产物引用。
    let body = body_of(&home, &session);
    assert_eq!(body.status, ToolResultStatus::Completed);
    assert_eq!(body.artifacts.len(), 1, "{:?}", body.artifacts);
    let artifact = &body.artifacts[0];
    assert_eq!(
        artifact.path,
        format!("artifacts/{run_a}/报告.md"),
        "引用路径相对会话目录"
    );
    assert_eq!(artifact.size, "产物正文\n".len() as u64);
    assert_eq!(
        artifact.hash,
        ContentHash::of_str("产物正文\n"),
        "哈希就是那份文件的字节"
    );
    assert_eq!(
        uri_of(artifact),
        format!("artifact://files/{run_a}/报告.md")
    );

    // 模型被喂回来的正文里印着**那一行产物入口**，而且 URI 就是从这一行取的——它手里
    // 只有这一条指路的，之后那次 `read` 用的就是它（§4.7）。
    let projected = fed_back(&llm);
    assert_eq!(projected.len(), 1, "{projected:?}");
    assert!(
        projected[0].0.contains(&format!(
            "产物：artifact://files/{run_a}/报告.md（{} B）",
            "产物正文\n".len()
        )),
        "正文里要印出产物入口与大小：{}",
        projected[0].0
    );
    let uri = produced_uri(&projected[0].0);
    assert_eq!(uri, format!("artifact://files/{run_a}/报告.md"));

    // 运行时**不复制第二份**：`artifact://files/…` 读的就是产出目录里那一份。
    let session_root = std::fs::canonicalize(home.session_dir(&session)).expect("会话目录在");
    assert_eq!(
        std::fs::read_to_string(session_root.join(&artifact.path)).expect("产出目录里有那份文件"),
        "产物正文\n"
    );

    gw.stop().await;

    // 重启：同一个数据目录，`service::start` 再来一次。那条 URI 照样读得回来，正文与
    // 第一次登记的那一份一致。
    let llm_b = FakeLlm::new(vec![vec![
        read_call(1, "pc-read", &uri),
        text_round(2, "读到了。"),
    ]]);
    let gw_b = home.start(Arc::clone(&llm_b) as Arc<dyn LlmClient>).await;
    let run_b = gw_b.submit(&session, "res-art-2", "读那份产物").await.run;
    let detail = gw_b.wait_terminal(&run_b).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm_b);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].1, "重启之后产物读得回来：{}", results[0].0);
    assert!(
        results[0].0.contains("产物正文"),
        "正文与第一次一致：{}",
        results[0].0
    );

    gw_b.stop().await;
}

// ---------------------------------------------------------------- 别的会话的 run

/// 会话边界：拿**另一个会话**的 `run` 拼出来的 `artifact://<run>/…` 读不到——那一份
/// 输出不属于这个会话。
#[tokio::test]
async fn another_sessions_run_is_out_of_reach() {
    let home = home_with(&config(&tools("\"read\", \"rg\"")));
    // 会话 A：读一个本地文件，于是有了它自己的 `tool-output/<run>/…`。
    let file = home.workspace().join("甲会话的文件.txt");
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    std::fs::write(&file, "只有甲会话读得到的内容\n").expect("写文件");

    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-a", "甲会话的文件.txt"),
        text_round(2, "读完了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session_a = gw.open_session().await;
    let run_a = gw.submit(&session_a, "res-a", "读我自己的文件").await.run;
    let detail = gw.wait_terminal(&run_a).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    // 甲会话那条输出的真实位置（`tool-output/<run>/<call>/<attempt>/output.json`）。
    let event = home
        .events(&session_a)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("有一条 tool.result");
    let secret =
        std::fs::read_to_string(home.session_dir(&session_a).join(event.output_ref.path()))
            .expect("output.json 读得到");
    assert!(secret.contains("只有甲会话读得到的内容"), "{secret}");
    let uri = format!(
        "artifact://{}/{}/{}/result",
        run_a, event.call_id, event.attempt_id
    );

    // 停机再起（同一个数据目录），开一个**新会话**，拿甲会话的 run 号拼出来的 URI 去读。
    gw.stop().await;
    let llm_b = FakeLlm::new(vec![vec![
        read_call(1, "pc-b", &uri),
        text_round(2, "读不到。"),
    ]]);
    let gw_b = home.start(Arc::clone(&llm_b) as Arc<dyn LlmClient>).await;
    let session_b = gw_b.open_session().await;
    assert_ne!(session_a, session_b);
    let run_b = gw_b.submit(&session_b, "res-b", "读别人的输出").await.run;
    let detail = gw_b.wait_terminal(&run_b).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm_b);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].1, "读不到就是错误结果：{}", results[0].0);
    assert!(
        !results[0].0.contains("只有甲会话读得到的内容"),
        "另一个会话的输出一个字都不该漏：{}",
        results[0].0
    );
    // 它不是"越权"：目标仍在本会话的只读根里，只是那儿没有这份文件——所以不经过审批。
    assert!(
        gw_b.interventions().await.is_empty(),
        "这条读不该停在任何需要操作者的东西上"
    );

    gw_b.stop().await;
}
