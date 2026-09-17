//! §14 的 Memory 覆盖那一行：「主动记录写入 Memos 后返回 ID / 链接，查询回到原文；
//! 删除或修改原文后更新关联摘要，写入未知时先核对。」
//!
//! 对着一个 **loopback 假 Memos**（`harness::start_memos`）跑，经过的是真东西：真
//! toolbox 里那份内置模块、真受管理解释器、真 HTTP 往返。本机没有 Memos 实例，而这一
//! 行验收要的恰恰是"一次真的写入之后还能按 ID 回到原文"。

use std::path::PathBuf;
use std::time::Duration;

use komo_kernel::traits::PythonHost;
use komo_kernel::types::plan::EnvVersion;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{CancelToken, PythonJob, PythonResult};
use komo_runtime::python_runtime::{PythonEnvConfig, PythonRuntime};
use komo_runtime::toolbox::{NullWriter, Toolbox, builtin};
use serde_json::{Value, json};

use crate::harness::*;

/// 一个指着假 Memos 的解释器。
///
/// `MEMOS_BASE_URL` / `MEMOS_TOKEN` 走 `resource_env`——**按名字透传**，值从进程环境取
/// 一次（§5.3）。这也是这一整组断言放在一个测试函数里的原因：进程环境是全局的。
fn interpreter(home: &std::path::Path, toolbox: &Toolbox) -> PythonRuntime {
    let mut config = PythonEnvConfig::new(home.join("python-envs/current"), home.to_path_buf());
    config.interpreter = Some(PathBuf::from("python3"));
    config.toolbox_parent = Some(toolbox.layout().parent());
    config.denied_imports = toolbox.layout().denied_import_roots();
    config.resource_env = vec!["MEMOS_BASE_URL".into(), "MEMOS_TOKEN".into()];
    config.timeout = Duration::from_secs(30);
    PythonRuntime::new(config, EnvVersion("py-test".into()))
}

async fn call(host: &PythonRuntime, function: &str, args: Value) -> PythonResult {
    let mut sink = NullWriter::detached();
    host.run(
        PythonJob::Call {
            module: "toolbox.memos".into(),
            function: function.into(),
            args,
        },
        &mut sink,
        CancelToken::new(),
    )
    .await
    .expect("宿主跑得起来")
}

/// §14 那一行的四句话，逐句。
#[tokio::test]
async fn the_four_things_the_acceptance_asks_of_memos() {
    if !have_python() {
        return;
    }
    let memos = start_memos().await;
    // SAFETY: 测试进程自己的环境。这两个变量只有这一个测试读写。
    unsafe {
        std::env::set_var("MEMOS_BASE_URL", &memos.base_url);
        std::env::set_var("MEMOS_TOKEN", MEMOS_TOKEN);
    }

    let dir = tempfile::tempdir().expect("临时目录");
    let toolbox = Toolbox::new(dir.path().join("toolbox"));
    toolbox.ensure_layout().expect("建目录");
    builtin::install(&toolbox, time::OffsetDateTime::now_utc()).expect("装内置模块");
    let host = interpreter(dir.path(), &toolbox);

    // 一、写入返回 ID 与原文链接。
    let written = call(&host, "create", json!({ "text": "牙医 周三 10:00" })).await;
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
    let read = call(&host, "get", json!({ "identifier": id })).await;
    assert_eq!(read.result["content"], json!("牙医 周三 10:00"));
    let found = call(&host, "search", json!({ "query": "牙医" })).await;
    let hits = found.result["memos"].as_array().expect("命中列表");
    assert_eq!(hits.len(), 1, "{found:?}");
    assert_eq!(hits[0]["content"], json!("牙医 周三 10:00"));
    assert_eq!(hits[0]["id"], json!(id));

    // 服务端**没有**实现 filter，模块因此必须自己再筛一遍——否则"查询"会把整库都
    // 端回来（§5.5「不能把 latest 文档当成该实例的接口保证」）。
    call(&host, "create", json!({ "text": "另一件毫不相干的事" })).await;
    let found = call(&host, "search", json!({ "query": "牙医" })).await;
    assert_eq!(
        found.result["memos"].as_array().expect("命中列表").len(),
        1,
        "{found:?}"
    );

    // 三、改原文 → 返回新内容与同一个 ID（关联摘要靠这两样更新）。
    let changed = call(
        &host,
        "update",
        json!({ "identifier": id, "text": "牙医 改到周五 15:00" }),
    )
    .await;
    assert_eq!(changed.result["id"], json!(id), "ID 不变");
    assert_eq!(changed.result["content"], json!("牙医 改到周五 15:00"));
    assert_eq!(
        call(&host, "get", json!({ "identifier": id })).await.result["content"],
        json!("牙医 改到周五 15:00"),
        "远端也改了"
    );

    // 四、写入结果未知 → **先核对，不直接重试**（§5.5、§8.6）。
    //
    // (a) 那一次写入其实没落地：核对说"确定未执行"，可以重做。
    let verdict = call(
        &host,
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
        "verify",
        json!({ "function": "create", "args": { "text": "响应丢了的那一条" } }),
    )
    .await;
    assert_eq!(verdict.result["kind"], json!("unknown"), "{verdict:?}");

    // 五、删除之后核对说"已达到"，读不回来。
    call(&host, "delete", json!({ "identifier": id })).await;
    let verdict = call(
        &host,
        "verify",
        json!({ "function": "delete", "args": { "identifier": id } }),
    )
    .await;
    assert_eq!(
        verdict.result["kind"],
        json!("already_satisfied"),
        "{verdict:?}"
    );
    let gone = call(&host, "get", json!({ "identifier": id })).await;
    assert_eq!(gone.status, ToolResultStatus::Failed, "{gone:?}");

    // 六、Memos 不可用时**明确失败**，不静默改存本地（§5.5）。
    unsafe {
        std::env::set_var("MEMOS_BASE_URL", "http://127.0.0.1:1");
    }
    let refused = call(&host, "create", json!({ "text": "连不上的时候" })).await;
    assert_eq!(refused.status, ToolResultStatus::Failed, "{refused:?}");
    assert!(
        refused.error.unwrap_or_default().contains("连不上"),
        "失败要说出来"
    );

    unsafe {
        std::env::remove_var("MEMOS_BASE_URL");
        std::env::remove_var("MEMOS_TOKEN");
    }
}
