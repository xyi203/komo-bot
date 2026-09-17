//! §14 的 Memory 覆盖那一行：「主动记录写入 Memos 后返回 ID / 链接，查询回到原文；
//! 删除或修改原文后更新关联摘要，写入未知时先核对。」
//!
//! 对着一个 **loopback 假 Memos**（`harness::start_memos`）跑，经过的是真东西：一台真
//! Gateway 装出来的 toolbox、真受管理解释器、真 HTTP 往返，以及 §5.3 那条凭证链
//! ——**地址与令牌只写在这台 Gateway 的 `.env` 里**，由 `service::python_env` 装上的
//! 解析口在每次 spawn 时按名取出来。测试自己一次 `std::env` 都不碰：那正是这条链子存在
//! 的理由。

use komo_gateway::service::test_support::harness::{FakeLlm, Home};
use komo_kernel::types::refs::ToolResultStatus;
use serde_json::json;

use crate::harness::*;

/// §14 那一行的四句话，逐句。
#[tokio::test]
async fn the_four_things_the_acceptance_asks_of_memos() {
    if !have_python() {
        return;
    }
    let memos = start_memos().await;
    let home = Home::new();
    write_env(
        home.path(),
        &[
            ("MEMOS_BASE_URL", &memos.base_url),
            ("MEMOS_TOKEN", MEMOS_TOKEN),
        ],
    );
    let gw = home.start(FakeLlm::finisher("好")).await;
    let (_toolbox, host) = interpreter(&gw);

    // 凭证**不在 Gateway 的进程环境里**：它只在 `.env` 里，按名解析进那一个子进程。
    assert!(
        std::env::var("MEMOS_TOKEN").is_err(),
        "凭证不该经过进程环境（§5.3）"
    );

    // 一、写入返回 ID 与原文链接。
    let written = call(
        &host,
        "toolbox.memos",
        "create",
        json!({ "text": "牙医 周三 10:00" }),
    )
    .await;
    assert_eq!(written.status, ToolResultStatus::Completed, "{written:?}");
    let id = written.result["id"].as_str().expect("有 ID").to_string();
    assert!(!id.is_empty());
    assert_eq!(
        written.result["url"],
        json!(format!("{}/m/{id}", memos.base_url)),
        "链接要指回原文"
    );
    assert_eq!(memos.contents(), vec!["牙医 周三 10:00"], "真的写进去了");

    // 二、查询回到原文——按 ID，也按内容。
    let read = call(&host, "toolbox.memos", "get", json!({ "identifier": id })).await;
    assert_eq!(read.result["content"], json!("牙医 周三 10:00"));
    let found = call(&host, "toolbox.memos", "search", json!({ "query": "牙医" })).await;
    let hits = found.result["memos"].as_array().expect("命中列表");
    assert_eq!(hits.len(), 1, "{found:?}");
    assert_eq!(hits[0]["content"], json!("牙医 周三 10:00"));
    assert_eq!(hits[0]["id"], json!(id));

    // 服务端**没有**实现 filter，模块因此必须自己再筛一遍——否则"查询"会把整库都
    // 端回来（§5.5「不能把 latest 文档当成该实例的接口保证」）。
    call(
        &host,
        "toolbox.memos",
        "create",
        json!({ "text": "另一件毫不相干的事" }),
    )
    .await;
    let found = call(&host, "toolbox.memos", "search", json!({ "query": "牙医" })).await;
    assert_eq!(
        found.result["memos"].as_array().expect("命中列表").len(),
        1,
        "{found:?}"
    );

    // 三、改原文 → 返回新内容与同一个 ID（关联摘要靠这两样更新）。
    let changed = call(
        &host,
        "toolbox.memos",
        "update",
        json!({ "identifier": id, "text": "牙医 改到周五 15:00" }),
    )
    .await;
    assert_eq!(changed.result["id"], json!(id), "ID 不变");
    assert_eq!(changed.result["content"], json!("牙医 改到周五 15:00"));
    assert_eq!(
        call(&host, "toolbox.memos", "get", json!({ "identifier": id }))
            .await
            .result["content"],
        json!("牙医 改到周五 15:00"),
        "远端也改了"
    );

    // 四、写入结果未知 → **先核对，不直接重试**（§5.5、§8.6）。
    //
    // (a) 那一次写入其实没落地：核对说"确定未执行"，可以重做。
    let verdict = call(
        &host,
        "toolbox.memos",
        "verify",
        json!({ "function": "create", "args": { "text": "一条根本没写成的记录" } }),
    )
    .await;
    assert_eq!(
        verdict.result["kind"],
        json!("not_performed"),
        "{verdict:?}"
    );

    // (b) 其实已经落地了（响应丢在路上）：核对说"已达到"，并给出 ID / 链接。
    let planted = memos.plant("响应丢了的那一条");
    let verdict = call(
        &host,
        "toolbox.memos",
        "verify",
        json!({ "function": "create", "args": { "text": "响应丢了的那一条" } }),
    )
    .await;
    assert_eq!(
        verdict.result["kind"],
        json!("already_satisfied"),
        "{verdict:?}"
    );
    assert!(
        verdict.result["evidence"]
            .as_str()
            .unwrap_or_default()
            .contains(&planted.to_string()),
        "证据要带上那条记录的 ID：{verdict:?}"
    );

    // (c) 内容一模一样的有两条：**不能**说"就是它"（§8.6「只有内容相似、时间接近，
    //     不能证明就是原调用创建的记录」）。
    memos.plant("响应丢了的那一条");
    let verdict = call(
        &host,
        "toolbox.memos",
        "verify",
        json!({ "function": "create", "args": { "text": "响应丢了的那一条" } }),
    )
    .await;
    assert_eq!(verdict.result["kind"], json!("unknown"), "{verdict:?}");

    // 五、删除之后核对说"已达到"，读不回来。
    call(
        &host,
        "toolbox.memos",
        "delete",
        json!({ "identifier": id }),
    )
    .await;
    let verdict = call(
        &host,
        "toolbox.memos",
        "verify",
        json!({ "function": "delete", "args": { "identifier": id } }),
    )
    .await;
    assert_eq!(
        verdict.result["kind"],
        json!("already_satisfied"),
        "{verdict:?}"
    );
    let gone = call(&host, "toolbox.memos", "get", json!({ "identifier": id })).await;
    assert_eq!(gone.status, ToolResultStatus::Failed, "{gone:?}");
}

/// ①③ Gateway 的进程环境里**没有** `MEMOS_TOKEN`，`.env` 里有 → 子进程读得到；
/// 而 `.env` 里其余的变量一个都不进子进程。
#[tokio::test]
async fn a_credential_travels_from_dot_env_to_the_child_and_nothing_else_does() {
    if !have_python() {
        return;
    }
    let memos = start_memos().await;
    let home = Home::new();
    write_env(
        home.path(),
        &[
            ("MEMOS_BASE_URL", &memos.base_url),
            ("MEMOS_TOKEN", MEMOS_TOKEN),
            // `.env` 里的另一条凭证。memos 没有声明它，所以它进不去。
            ("UNRELATED_SECRET", "must-not-leak"),
        ],
    );
    let gw = home.start(FakeLlm::finisher("好")).await;
    let (toolbox, host) = interpreter(&gw);

    assert!(std::env::var("MEMOS_TOKEN").is_err());
    assert!(std::env::var("UNRELATED_SECRET").is_err());

    // 写得进去 = 令牌真的到了子进程（假 Memos 会校验 Bearer）。
    let written = call(
        &host,
        "toolbox.memos",
        "create",
        json!({ "text": "从 .env 来的令牌" }),
    )
    .await;
    assert_eq!(written.status, ToolResultStatus::Completed, "{written:?}");

    // 装一个只声明 `MEMOS_BASE_URL` 的模块，让它把子进程看得见的东西报回来。
    toolbox
        .install_builtin("probe", PROBE, None, time::OffsetDateTime::now_utc())
        .expect("装得上")
        .expect("toolbox 里本来没有它");
    let seen = call(
        &host,
        "toolbox.probe",
        "env",
        json!({ "names": ["MEMOS_BASE_URL", "MEMOS_TOKEN", "UNRELATED_SECRET", "KOMO_LLM_API_KEY"] }),
    )
    .await;
    assert_eq!(
        seen.result,
        json!({
            "MEMOS_BASE_URL": memos.base_url,
            // 声明的只有 BASE_URL：另一个模块的令牌、无关的凭证、模型钥匙都不在。
            "MEMOS_TOKEN": null,
            "UNRELATED_SECRET": null,
            "KOMO_LLM_API_KEY": null,
        }),
        "{seen:?}"
    );
}

/// ② `.env` 改完 + `reload` → **下一次调用**就是新值，不必重启（§3 第 2 步）。
#[tokio::test]
async fn rotating_the_token_in_dot_env_takes_effect_on_the_next_call() {
    if !have_python() {
        return;
    }
    let memos = start_memos().await;
    let home = Home::new();
    write_env(
        home.path(),
        &[
            ("MEMOS_BASE_URL", &memos.base_url),
            ("MEMOS_TOKEN", MEMOS_TOKEN),
        ],
    );
    let gw = home.start(FakeLlm::finisher("好")).await;
    let (_toolbox, host) = interpreter(&gw);
    assert_eq!(
        call(
            &host,
            "toolbox.memos",
            "create",
            json!({ "text": "旧令牌" })
        )
        .await
        .status,
        ToolResultStatus::Completed
    );

    // 远端换了令牌；`.env` 还没改 → 现在这一次应该被拒。
    memos.rotate_token("rotated-token");
    let refused = call(
        &host,
        "toolbox.memos",
        "create",
        json!({ "text": "旧令牌还在用" }),
    )
    .await;
    assert_eq!(refused.status, ToolResultStatus::Failed, "{refused:?}");
    assert!(
        refused.error.unwrap_or_default().contains("401"),
        "远端拒了它"
    );

    // 改 `.env` + reload → 下一次调用用新令牌。**没有重启 Gateway。**
    write_env(
        home.path(),
        &[
            ("MEMOS_BASE_URL", &memos.base_url),
            ("MEMOS_TOKEN", "rotated-token"),
        ],
    );
    reload(&gw);
    let accepted = call(
        &host,
        "toolbox.memos",
        "create",
        json!({ "text": "新令牌" }),
    )
    .await;
    assert_eq!(accepted.status, ToolResultStatus::Completed, "{accepted:?}");
    assert!(memos.contents().iter().any(|text| text == "新令牌"));
}

/// ④ 声明了、`.env` 里却没有 → 那个变量**不设置**，模块自己报得出"未配置"。
#[tokio::test]
async fn a_credential_missing_from_dot_env_is_left_unset_and_the_module_says_so() {
    if !have_python() {
        return;
    }
    let memos = start_memos().await;
    let home = Home::new();
    // 只给地址，不给令牌。
    write_env(home.path(), &[("MEMOS_BASE_URL", &memos.base_url)]);
    let gw = home.start(FakeLlm::finisher("好")).await;
    let (_toolbox, host) = interpreter(&gw);

    let refused = call(
        &host,
        "toolbox.memos",
        "create",
        json!({ "text": "没有令牌" }),
    )
    .await;
    assert_eq!(refused.status, ToolResultStatus::Failed, "{refused:?}");
    let message = refused.error.unwrap_or_default();
    assert!(message.contains("MEMOS_TOKEN"), "{message}");
    assert!(message.contains("没有配置"), "{message}");
    assert!(memos.contents().is_empty(), "一条都不该写进去");

    // 补上 + reload → 同一次调用现在成了。
    write_env(
        home.path(),
        &[
            ("MEMOS_BASE_URL", &memos.base_url),
            ("MEMOS_TOKEN", MEMOS_TOKEN),
        ],
    );
    reload(&gw);
    assert_eq!(
        call(
            &host,
            "toolbox.memos",
            "create",
            json!({ "text": "补上之后" })
        )
        .await
        .status,
        ToolResultStatus::Completed
    );
}

/// 一个只声明 `MEMOS_BASE_URL` 的模块，把子进程看得见的变量报回来。
const PROBE: &str = r#""""报告子进程看得见哪些变量。测试用。"""

import os

__all__ = ["env"]
__komo_env__ = ["MEMOS_BASE_URL"]


def env(names):
    return {name: os.environ.get(name) for name in names}
"#;
