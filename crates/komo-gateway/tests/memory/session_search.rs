//! Agent 在 Run 里自己查记忆（`komo memory search` 带 `KOMO_SESSION`，§9.4「Agent 主动查」）：
//! Gateway 按这个会话的 Profile 放行作用域，与自动召回同一处判断；`memory = off` 的会话被拒。

use komo_kernel::types::ids::SessionId;
use komo_kernel::types::memory::MemoryScope;

use crate::Scripted;
use crate::harness::*;
use crate::scoped;

/// 四个 Agent：默认的（不限作用域）、`coder`（`project:acme` + 共享资料）、`helper`（私有）、
/// `quiet`（关了记忆）。
fn config() -> String {
    memory_config("keyword", false).replace(
        "[agents.assistant]",
        "[agents.assistant]\n\n[agents.coder]\nmemory_scope = \"project:acme\"\n\n\
         [agents.helper]\nmemory = \"private\"\n\n[agents.quiet]\nmemory = \"off\"",
    )
}

async fn session_of(gateway: &TestGateway, agent: &str) -> SessionId {
    let core = komo_gateway::service::test_support::driver_stream::core(gateway);
    SessionId::from_raw(core.main_session(agent).await.unwrap().0.id)
}

async fn found(gateway: &TestGateway, session: &SessionId) -> Vec<String> {
    let (status, body) = gateway
        .get_json(&format!(
            "/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=keyword&session={session}"
        ))
        .await;
    assert_eq!(status, 200, "{body}");
    let mut ids: Vec<String> = body["memories"]
        .as_array()
        .expect("数组")
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

async fn start() -> TestGateway {
    let gateway = super::gateway(&config())
        .scripted(komo_kernel::test_support::ScriptedLlm::new(vec![]))
        .start()
        .await;
    for item in [
        scoped("m-personal", "客厅空调设 26 度", MemoryScope::Personal),
        scoped(
            "m-acme",
            "acme 的空调遥测在 26 度",
            MemoryScope::Project {
                project_id: "acme".into(),
            },
        ),
        scoped(
            "m-other",
            "别的项目的空调日志在 26 度",
            MemoryScope::Project {
                project_id: "other".into(),
            },
        ),
        scoped(
            "m-helper",
            "helper 私有的空调口令",
            MemoryScope::Agent {
                agent_id: "helper".into(),
            },
        ),
        scoped(
            "m-secret",
            "另一个 Agent 私有的空调口令",
            MemoryScope::Agent {
                agent_id: "secret".into(),
            },
        ),
    ] {
        gateway.state().memory.put(item, None).await.unwrap();
    }
    gateway
}

/// 带会话的查询只返回该会话放行作用域里的条目：与自动召回同一处判断。
#[tokio::test]
async fn a_session_search_returns_only_the_scopes_its_agent_may_see() {
    let gateway = start().await;

    // 不限作用域的 Agent：看得到共享的三条，但没有任何 `agent:*`。
    let assistant = session_of(&gateway, "assistant").await;
    assert_eq!(
        found(&gateway, &assistant).await,
        vec!["m-acme", "m-other", "m-personal"]
    );
    // 写了 `project:acme` 的 Agent：它那一份，外加共享的用户资料。
    let coder = session_of(&gateway, "coder").await;
    assert_eq!(found(&gateway, &coder).await, vec!["m-acme", "m-personal"]);

    // 不带会话（操作者终端）：照旧不按会话限制。
    let (status, body) = gateway
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=keyword")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"].as_array().unwrap().len(), 3, "{body}");
}

/// `private` 的 Agent 查不到别的 Agent 的私有记忆，也查不到共享的；只有自己那份。
#[tokio::test]
async fn a_private_agent_cannot_search_another_agents_private_memory() {
    let gateway = start().await;
    let helper = session_of(&gateway, "helper").await;
    assert_eq!(found(&gateway, &helper).await, vec!["m-helper"]);

    // 明确点名别人的私有作用域也不行。
    let (status, body) = gateway
        .get_json(&format!(
            "/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=keyword&scope=agent:secret&session={helper}"
        ))
        .await;
    assert_ne!(status, 200, "{body}");
}

/// `memory = off` 的会话不能查记忆。
#[tokio::test]
async fn a_session_whose_memory_is_off_cannot_search() {
    let gateway = start().await;
    let quiet = session_of(&gateway, "quiet").await;
    let (status, body) = gateway
        .get_json(&format!(
            "/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=keyword&session={quiet}"
        ))
        .await;
    assert_ne!(status, 200, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("memory = off"),
        "{body}"
    );
}

/// 不带查询词的清单同样只给会话放行的作用域。
#[tokio::test]
async fn a_session_listing_is_scoped_too() {
    let gateway = start().await;
    let helper = session_of(&gateway, "helper").await;
    let (status, body) = gateway
        .get_json(&format!("/v1/memories?session={helper}"))
        .await;
    assert_eq!(status, 200, "{body}");
    let ids: Vec<&str> = body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["m-helper"]);
}
