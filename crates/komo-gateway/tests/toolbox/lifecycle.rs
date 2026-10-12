//! §5.4 的迭代流程，一步一个测试：候选 → 跑测试 → 启用 → 之后的 `call` 用
//! **已启用且已测试**的那一版。启用 / 停用直接生效（§7 删掉审批之后没有 Pending）。

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

/// §14 阶段 6：**调用使用已测试版本**。走完整条路：写候选 → 跑测试 → 启用（当场
/// 装上）→ 清单里的当前版本就是测过的那一版。
#[tokio::test]
async fn a_candidate_becomes_callable_only_after_it_is_tested() {
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

    // 4. 启用 → 当场装上，回执里就是装上的那一版。
    let (code, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    assert_eq!(code, 200, "{body}");
    let change: serde_json::Value = serde_json::from_str(&body).expect("响应");
    assert_eq!(change["status"], "applied", "{body}");
    assert_eq!(change["version"], json!(report.version), "{body}");
    assert!(change.get("approval").is_none(), "没有审批号：{body}");

    let info = toolbox_show(&gw, "adder").await;
    let enabled = info.enabled.expect("已启用");
    assert_eq!(enabled.version, report.version, "装上的就是测过的那一版");
    assert_eq!(info.exports, vec!["add"]);
    // 候选已经成为当前版本，`.staging` 清空。
    assert!(info.candidate.is_none());
}

/// 装上之后没有新候选，再启用一次是一句说得清的 400，不是把当前版本再装一遍。
#[tokio::test]
async fn enabling_again_without_a_new_candidate_is_refused_cleanly() {
    if !have_python() {
        return;
    }
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    write_candidate(home.path(), "adder", ADDER, Some(ADDER_TEST));
    assert!(toolbox_test(&gw, "adder").await.passed);
    let (code, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    assert_eq!(code, 200, "{body}");

    let (code, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    assert_eq!(code, 400, "{body}");
    assert!(
        toolbox_show(&gw, "adder").await.enabled.is_some(),
        "已经装上的那一版不受影响"
    );
}

/// 「校验候选哈希与已测版本一致」（§5.4）：测完之后有人改了候选，**测过的是那一版，
/// 不是"这个模块的下一版"**——点名旧版本是 409，不点名是 422（先去测），都不装。
#[tokio::test]
async fn a_candidate_edited_after_its_test_does_not_ride_in_on_the_old_test() {
    if !have_python() {
        return;
    }
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    write_candidate(home.path(), "adder", ADDER, Some(ADDER_TEST));
    let tested_version = toolbox_test(&gw, "adder").await.version;

    // 测完之后又改了一版（而且这一版没测过）。
    write_candidate(
        home.path(),
        "adder",
        &format!("{ADDER}\n# 偷偷加的\n"),
        Some(ADDER_TEST),
    );

    let (code, body) = gw
        .post(
            "/v1/toolbox/adder/enable",
            json!({ "version": tested_version }),
        )
        .await;
    assert_eq!(code, 409, "点名的那一版已经不是候选：{body}");

    let (code, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    assert_eq!(code, 422, "新的这一版没测过：{body}");
    assert!(
        toolbox_show(&gw, "adder").await.enabled.is_none(),
        "什么都不该装上"
    );
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

/// 停用同样当场生效：回执是 `applied`，清单里它不再是已启用。
///
/// 用一个自己写的模块而不是内置的 memos：内置模块缺了就会在下一次打开 toolbox 时被重新
/// 装回去（`service::toolbox_of`，见报告）。
#[tokio::test]
async fn disabling_a_module_applies_at_once() {
    if !have_python() {
        return;
    }
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    write_candidate(home.path(), "adder", ADDER, Some(ADDER_TEST));
    assert!(toolbox_test(&gw, "adder").await.passed);
    let (code, body) = gw.post("/v1/toolbox/adder/enable", json!({})).await;
    assert_eq!(code, 200, "{body}");

    let (code, body) = gw.post("/v1/toolbox/adder/disable", json!({})).await;
    assert_eq!(code, 200, "{body}");
    let change: serde_json::Value = serde_json::from_str(&body).expect("响应");
    assert_eq!(change["status"], "applied", "{body}");
    // 正文移出了 toolbox/，又没有候选：这个名字此刻什么都指不到。
    let (code, body) = gw.get("/v1/toolbox/adder").await;
    assert_eq!(code, 404, "停用之后它不再是已启用：{body}");
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
