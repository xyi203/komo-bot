//! §14 阶段 6 最后一行：**toolbox 启用的审批在微信里能看到版本差异与测试结果**。
//!
//! 三个渠道各渲染一遍：渠道之间的差别只在渲染（§11.3），所以"看得见"这件事要在三处
//! 都成立，不能只在组装 `ApprovalPresentation` 的那一侧成立。

use komo_gateway::render;
use komo_gateway::service::test_support::harness::{FakeLlm, Home};
use komo_kernel::types::chat::ApprovalPresentation;
use serde_json::json;

use crate::harness::*;

const V1: &str = r#""""问候。"""

__all__ = ["greet"]


def greet(name):
    return "hi " + name
"#;

const V2: &str = r#""""问候。"""

__all__ = ["greet"]


def greet(name):
    return "你好，" + name
"#;

const GREET_TEST: &str = r#"import unittest

from toolbox.greeter import greet


class GreetTest(unittest.TestCase):
    def test_greets(self):
        self.assertIn("A", greet("A"))
"#;

/// 从 v1 升到 v2 的那条审批请求。
async fn upgrade_request() -> (Home, ApprovalPresentation) {
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;

    // v1 先装上。
    write_candidate(home.path(), "greeter", V1, Some(GREET_TEST));
    assert!(toolbox_test(&gw, "greeter").await.passed);
    let (_, body) = gw.post("/v1/toolbox/greeter/enable", json!({})).await;
    let change: serde_json::Value = serde_json::from_str(&body).expect("响应");
    let approval = change["approval"].as_str().expect("审批号").to_string();
    gw.post(
        &format!("/v1/approvals/{approval}/decision"),
        json!({ "approved": true, "scope": "once" }),
    )
    .await;
    eventually("greeter v1 装上", || async {
        toolbox_show(&gw, "greeter").await.enabled.is_some()
    })
    .await;

    // v2 的候选 + 测试 + 启用请求。
    write_candidate(home.path(), "greeter", V2, Some(GREET_TEST));
    assert!(toolbox_test(&gw, "greeter").await.passed);
    let (code, body) = gw.post("/v1/toolbox/greeter/enable", json!({})).await;
    assert_eq!(code, 200, "{body}");

    // 请求投到了 home chat。取**最后**那一条：前面还有 v1 那次启用的请求，而这里要
    // 看的是"从 v1 升到 v2"的那份差异。
    let presentation = gw.home.approvals().pop().expect("审批请求投到了 home chat");
    drop(gw);
    (home, presentation)
}

#[tokio::test]
async fn the_wechat_message_shows_the_version_diff_and_the_test_result() {
    if !have_python() {
        return;
    }
    let (_home, presentation) = upgrade_request().await;
    let text = render::wechat::approval_text(&presentation);

    // 版本差异。
    assert!(text.contains("改动"), "{text}");
    assert!(text.contains("greeter"), "{text}");
    assert!(text.contains("- "), "旧的那一行：{text}");
    assert!(text.contains("+     return \"你好，\" + name"), "{text}");
    // 测试结果。
    assert!(text.contains("已有验证"), "{text}");
    assert!(text.contains("候选测试通过"), "{text}");
    // 答它的那条命令。
    assert!(text.contains("/approve"), "{text}");
}

#[tokio::test]
async fn the_telegram_message_shows_them_too() {
    if !have_python() {
        return;
    }
    let (_home, presentation) = upgrade_request().await;
    let text = render::telegram::approval_text(&presentation);
    assert!(text.contains("改动"), "{text}");
    assert!(text.contains("已有验证"), "{text}");
    assert!(text.contains("候选测试通过"), "{text}");
    assert!(text.contains("你好"), "{text}");
}

#[tokio::test]
async fn the_feishu_card_shows_them_too() {
    if !have_python() {
        return;
    }
    let (_home, presentation) = upgrade_request().await;
    let card = render::feishu::approval_card(&presentation);
    let printed = serde_json::to_string(&card).expect("卡片");
    assert!(printed.contains("改动"), "{printed}");
    assert!(printed.contains("已有验证"), "{printed}");
    assert!(printed.contains("候选测试通过"), "{printed}");
    assert!(printed.contains("你好"), "版本差异也在卡片里：{printed}");
}

/// 审批消息里的「动作」那一项要说得出这是哪个模块、哪一版——`ExecutionPlan` 绑的正是
/// 它们（§7.2）。
#[tokio::test]
async fn the_plan_behind_the_request_binds_the_module_and_the_target_version() {
    if !have_python() {
        return;
    }
    let (_home, presentation) = upgrade_request().await;
    assert_eq!(
        presentation.plan.operation,
        komo_kernel::types::plan::Operation::ToolboxChange {
            module: "toolbox.greeter".into()
        }
    );
    assert!(
        presentation.plan.versions.module.is_some(),
        "目标版本要绑在计划上（§7.2）"
    );
    // 范围只给"本次"：一条"今后任意 toolbox 变更都行"的授权是 §7.2 明确要挡的。
    assert_eq!(
        presentation.scopes,
        vec![komo_kernel::types::chat::ApprovalScope::Once]
    );
}
