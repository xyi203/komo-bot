//! §5.4 的迭代流程，一步一个测试：候选 → 跑测试 → 审核 → 启用 → 之后的 `call` 用
//! **已启用且已测试**的那一版。

use komo_gateway::service::test_support::harness::{FakeLlm, Home};
use serde_json::json;

use crate::harness::*;

const ADDER: &str = r#""""把两个数加起来。"""

__all__ = ["add"]


def add(a, b):
    return a + b
"#;

const ADDER_TEST: &str = r#"import unittest

from toolbox.adder import add


class AddTest(unittest.TestCase):
    def test_adds(self):
        self.assertEqual(add(2, 3), 5)
"#;

/// §14 阶段 6：**调用使用已批准且已测试版本**。走完整条路：写候选 → 跑测试 →
/// 启用（产生审批）→ 批准 → 装上 → `python` 的 `call` 计划绑的就是这一版。
#[tokio::test]
async fn a_candidate_becomes_callable_only_after_it_is_tested_and_approved() {
    if !have_python() {
        return;
    }
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;

    // 1. 候选。这时候它既不在清单的"已启用"里，也调不到。
    write_candidate(home.path(), "adder", ADDER, Some(ADDER_TEST));
    let info = toolbox_show(&gw, "adder").await;
    assert!(info.enabled.is_none(), "候选不是已启用");
    assert!(info.candidate.is_some());
    assert!(info.exports.is_empty(), "没启用就没有导出可言");

    // 2. 还没测过就想启用：被挡下来，而且说得出该做什么。
    let (code, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    assert_eq!(code, 422, "{body}");
    assert!(body.contains("komo toolbox test"), "{body}");

    // 3. 跑候选测试。
    let report = toolbox_test(&gw, "adder").await;
    assert!(report.passed, "{report:?}");
    assert_eq!(report.ran, 1);

    // 4. 启用 → 一条审批。**不是**直接装上。
    let (code, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    assert_eq!(code, 200, "{body}");
    let change: serde_json::Value = serde_json::from_str(&body).expect("响应");
    assert_eq!(change["status"], "pending", "{body}");
    assert!(
        toolbox_show(&gw, "adder").await.enabled.is_none(),
        "没批准之前一秒钟都不该是当前版本"
    );

    // 5. 批准 → 后台把它装上。
    let approval = change["approval"].as_str().expect("审批号").to_string();
    let (code, body) = gw
        .post(
            &format!("/v1/approvals/{approval}/decision"),
            json!({ "approved": true, "scope": "once" }),
        )
        .await;
    assert_eq!(code, 200, "{body}");

    eventually("adder 装上", || async {
        toolbox_show(&gw, "adder").await.enabled.is_some()
    })
    .await;

    let info = toolbox_show(&gw, "adder").await;
    let enabled = info.enabled.expect("已启用");
    assert_eq!(enabled.version, report.version, "装上的就是测过的那一版");
    assert_eq!(info.exports, vec!["add"]);
    // 候选已经成为当前版本，`.staging` 清空。
    assert!(toolbox_show(&gw, "adder").await.candidate.is_none());
}

/// 拒绝就是拒绝：模块不装，`.staging` 里的候选原样留着（改完再问一次是下一步）。
#[tokio::test]
async fn a_rejected_enable_leaves_the_module_alone_and_keeps_the_candidate() {
    if !have_python() {
        return;
    }
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    write_candidate(home.path(), "adder", ADDER, Some(ADDER_TEST));
    assert!(toolbox_test(&gw, "adder").await.passed);

    let (_, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    let change: serde_json::Value = serde_json::from_str(&body).expect("响应");
    let approval = change["approval"].as_str().expect("审批号").to_string();
    gw.post(
        &format!("/v1/approvals/{approval}/decision"),
        json!({ "approved": false, "scope": "once" }),
    )
    .await;

    // 给后台任务一点时间去看见那个"拒绝"。
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let info = toolbox_show(&gw, "adder").await;
    assert!(info.enabled.is_none(), "拒绝了就不该装上");
    assert!(info.candidate.is_some(), "候选留着，改完可以再问一次");

    // 再问一次：拿到一条**新的**审批，不是把上一条的"拒绝"当答案。
    let (code, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    assert_eq!(code, 200, "{body}");
    let again: serde_json::Value = serde_json::from_str(&body).expect("响应");
    assert_eq!(again["status"], "pending", "{body}");
    assert_ne!(again["approval"], change["approval"]);
}

/// 「校验候选哈希与已测版本一致」（§5.4）：审批还等着的时候有人改了候选，批准之后
/// 装不上去——**批准的是那一版，不是"这个模块的下一版"**。
#[tokio::test]
async fn a_candidate_edited_during_the_approval_window_does_not_ride_in_on_the_old_approval() {
    if !have_python() {
        return;
    }
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    write_candidate(home.path(), "adder", ADDER, Some(ADDER_TEST));
    let approved_version = toolbox_test(&gw, "adder").await.version;

    let (_, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    let change: serde_json::Value = serde_json::from_str(&body).expect("响应");
    assert_eq!(change["version"], json!(approved_version));
    let approval = change["approval"].as_str().expect("审批号").to_string();

    // 审批窗口里又改了一版（而且这一版没测过）。
    write_candidate(
        home.path(),
        "adder",
        &format!("{ADDER}\n# 偷偷加的\n"),
        Some(ADDER_TEST),
    );

    gw.post(
        &format!("/v1/approvals/{approval}/decision"),
        json!({ "approved": true, "scope": "once" }),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    assert!(
        toolbox_show(&gw, "adder").await.enabled.is_none(),
        "批准的那一版已经不在了，什么都不该装上"
    );
    // 操作者在 home chat 里看得见"没能装上"，不是一片安静。
    let said = gw.home.texts().join("\n");
    assert!(said.contains("没能装上"), "{said}");
}

/// 内置的 memos 随 Gateway 装进 toolbox 并**已启用**（§5.5）。
#[tokio::test]
async fn the_builtin_memos_module_is_there_on_first_boot() {
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;

    let modules = toolbox_list(&gw).await;
    let memos = modules
        .iter()
        .find(|module| module.module == "memos")
        .expect("memos 在清单里");
    let enabled = memos.enabled.as_ref().expect("已启用");
    assert!(enabled.builtin, "它是随 komo 装进来的");
    assert_eq!(
        memos.exports,
        vec!["create", "get", "search", "update", "delete", "verify"]
    );
    assert_eq!(memos.verifier.as_deref(), Some("verify"));
    // 凭证**引用**看得见，凭证本身一个字都不在（§5.3）。
    assert_eq!(memos.env, vec!["MEMOS_BASE_URL", "MEMOS_TOKEN"]);
    let printed = serde_json::to_string(&memos).expect("序列化");
    assert!(!printed.contains(MEMOS_TOKEN), "{printed}");
}

/// 停用也要过同一道门：它一样是"修改启用中的 toolbox"。
#[tokio::test]
async fn disabling_a_module_asks_too() {
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;

    let (code, body) = gw.post("/v1/toolbox/memos/disable", json!({})).await;
    assert_eq!(code, 200, "{body}");
    let change: serde_json::Value = serde_json::from_str(&body).expect("响应");
    assert_eq!(change["status"], "pending", "{body}");
    assert!(
        toolbox_show(&gw, "memos").await.enabled.is_some(),
        "没批准之前它还在"
    );
}

/// 说不出名字的模块给一个说得清的 404，不是 500。
#[tokio::test]
async fn a_module_nobody_wrote_is_a_clean_not_found() {
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    let (code, body) = gw.get("/v1/toolbox/ghost").await;
    assert_eq!(code, 404, "{body}");
    let (code, body) = gw.get("/v1/toolbox/..%2Fetc").await;
    assert!(code == 400 || code == 404, "{code}：{body}");
}
