//! Router 判断的验收：假模型，不发真实请求。

use std::sync::Mutex;

use super::*;

fn task(id: &str, state: CandidateState) -> CandidateTask {
    CandidateTask {
        id: id.into(),
        title: format!("任务 {id}"),
        state,
        last_said: "我先把这份报告整理出来".into(),
    }
}

fn input(scope: RouteScope, message: &str) -> RouterInput {
    RouterInput::new(
        scope,
        message,
        vec![
            task("t1", CandidateState::Running),
            task("t2", CandidateState::Finished),
        ],
        vec![RecentMessage {
            speaker: Speaker::Operator,
            text: "帮我看下周报".into(),
        }],
        RouterLimits::default(),
    )
}

/// 按脚本一条条回答，并记下被调了几次、收到的提示。
struct Scripted {
    replies: Mutex<Vec<Result<String, String>>>,
    prompts: Mutex<Vec<RouterPrompt>>,
}

impl Scripted {
    fn new(replies: Vec<Result<&str, &str>>) -> Scripted {
        Scripted {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .rev()
                    .map(|reply| reply.map(str::to_string).map_err(str::to_string))
                    .collect(),
            ),
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

#[async_trait]
impl RouterModel for Scripted {
    async fn complete(&self, prompt: &RouterPrompt) -> Result<String, String> {
        self.prompts.lock().unwrap().push(prompt.clone());
        self.replies
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Err("脚本用完了".into()))
    }
}

async fn run(input: &RouterInput, replies: Vec<Result<&str, &str>>) -> (Routed, usize) {
    let model = Scripted::new(replies);
    let routed = route(input, &model).await;
    (routed, model.calls())
}

#[tokio::test]
async fn a_private_message_can_be_routed_to_all_four_destinations() {
    let input = input(RouteScope::Private, "把周报发我");
    let cases = [
        (
            r#"{"route":"new","description":"把周报发给我","title":"发周报"}"#,
            Route::New {
                description: "把周报发给我".into(),
                title: "发周报".into(),
            },
        ),
        (
            r#"{"route":"continue","task_id":"t1"}"#,
            Route::Continue { task: "t1".into() },
        ),
        (
            r#"{"route":"stop","task_id":"t2"}"#,
            Route::Stop { task: "t2".into() },
        ),
        (r#"{"route":"list"}"#, Route::List),
    ];
    for (reply, expected) in cases {
        let (routed, calls) = run(&input, vec![Ok(reply)]).await;
        assert_eq!(routed.route, expected, "{reply}");
        assert_eq!(routed.fallback, None, "{reply}");
        assert_eq!(calls, 1);
    }
}

#[tokio::test]
async fn a_group_message_can_be_routed_to_new_continue_stop_and_ignore() {
    let input = input(RouteScope::Group, "@bot 看下这个 MR");
    for (reply, expected) in [
        (
            r#"{"route":"new","description":"看下这个 MR","title":"看 MR"}"#,
            Route::New {
                description: "看下这个 MR".into(),
                title: "看 MR".into(),
            },
        ),
        (
            r#"{"route":"continue","task_id":"t1"}"#,
            Route::Continue { task: "t1".into() },
        ),
        (
            r#"{"route":"stop","task_id":"t1"}"#,
            Route::Stop { task: "t1".into() },
        ),
        (r#"{"route":"ignore"}"#, Route::Ignore),
    ] {
        let (routed, _) = run(&input, vec![Ok(reply)]).await;
        assert_eq!((routed.route, routed.fallback), (expected, None), "{reply}");
    }
}

#[tokio::test]
async fn a_group_list_answer_is_a_failure_that_falls_back_to_ignore() {
    let input = input(RouteScope::Group, "你手上有哪些事");
    let (routed, calls) = run(
        &input,
        vec![Ok(r#"{"route":"list"}"#), Ok(r#"{"route":"list"}"#)],
    )
    .await;
    assert_eq!(routed.route, Route::Ignore);
    assert_eq!(
        routed.fallback.map(|fallback| fallback.reason),
        Some(RouteFailure::NotAllowed("list".into()))
    );
    assert_eq!(calls, 2);
}

#[tokio::test]
async fn a_private_ignore_answer_is_a_failure_that_falls_back_to_new() {
    let input = input(RouteScope::Private, "随便聊聊");
    let (routed, _) = run(&input, vec![Ok(r#"{"route":"ignore"}"#); 2]).await;
    assert!(matches!(routed.route, Route::New { .. }));
    assert!(matches!(
        routed.fallback.map(|fallback| fallback.reason),
        Some(RouteFailure::NotAllowed(_))
    ));
}

#[tokio::test]
async fn pointing_at_a_task_outside_the_candidates_falls_back_per_scope() {
    let reply = r#"{"route":"continue","task_id":"ghost"}"#;
    let (private, _) = run(&input(RouteScope::Private, "接着做"), vec![Ok(reply); 2]).await;
    assert_eq!(
        private.route,
        Route::New {
            description: "接着做".into(),
            title: "接着做".into()
        }
    );
    assert_eq!(
        private.fallback.map(|fallback| fallback.reason),
        Some(RouteFailure::UnknownTask("ghost".into()))
    );

    let (group, _) = run(&input(RouteScope::Group, "接着做"), vec![Ok(reply); 2]).await;
    assert_eq!(group.route, Route::Ignore);

    let stop = r#"{"route":"stop","task_id":"ghost"}"#;
    let (group, _) = run(&input(RouteScope::Group, "停掉"), vec![Ok(stop); 2]).await;
    assert_eq!(group.route, Route::Ignore);
    assert!(group.fallback.is_some());
}

#[tokio::test]
async fn a_model_error_is_retried_up_to_the_limit_and_then_falls_back() {
    let input = input(RouteScope::Private, "查一下今天的告警");
    let model = Scripted::new(vec![Err("503"), Err("503"), Err("503"), Err("503")]);
    let routed = route(&input, &model).await;
    assert_eq!(model.calls(), MAX_ATTEMPTS as usize, "不能超过重试上限");
    assert_eq!(
        routed.fallback,
        Some(Fallback {
            reason: RouteFailure::Model("503".into()),
            attempts: MAX_ATTEMPTS
        })
    );
    assert_eq!(
        routed.route,
        Route::New {
            description: "查一下今天的告警".into(),
            title: "查一下今天的告警".into()
        }
    );
}

#[tokio::test]
async fn a_malformed_answer_is_retried_and_a_good_second_answer_is_used() {
    let input = input(RouteScope::Private, "停掉 t1");
    let (routed, calls) = run(
        &input,
        vec![
            Ok("我觉得应该停掉它"),
            Ok(r#"{"route":"stop","task_id":"t1"}"#),
        ],
    )
    .await;
    assert_eq!(calls, 2);
    assert_eq!(routed.route, Route::Stop { task: "t1".into() });
    assert_eq!(routed.fallback, None, "重试成功就不是兜底");
}

#[tokio::test]
async fn malformed_answers_fall_back_without_exceeding_the_limit() {
    let input = input(RouteScope::Private, "hello");
    for bad in [
        "不是 JSON",
        r#"{"route":"new","description":"x"}"#,
        r#"{"route":"new","description":" ","title":"t"}"#,
        r#"{"route":"continue"}"#,
        r#"{"route":"fly"}"#,
        r#"{"no_route":1}"#,
        r#"{"route":"new","description":"x","title":"#,
    ] {
        let model = Scripted::new(vec![Ok(bad); 5]);
        let routed = route(&input, &model).await;
        assert!(model.calls() <= MAX_ATTEMPTS as usize, "{bad}");
        assert!(
            matches!(
                routed.fallback.map(|fallback| fallback.reason),
                Some(RouteFailure::Malformed(_))
            ),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn the_fallback_new_keeps_the_original_message_as_the_description() {
    let message = "  帮我把 /tmp/a.log 里的错误统计一下\n\n再按小时分组  ";
    let (routed, _) = run(&input(RouteScope::Private, message), vec![Err("x"); 2]).await;
    let Route::New { description, title } = routed.route else {
        panic!("{:?}", routed.route)
    };
    assert_eq!(description, message, "任务说明就是消息原文，一字不改");
    assert_eq!(title, "帮我把 /tmp/a.log 里的错误统计一下");

    let long = "长".repeat(TITLE_CHARS + 10);
    let (routed, _) = run(&input(RouteScope::Private, &long), vec![Err("x"); 2]).await;
    let Route::New { title, .. } = routed.route else {
        panic!()
    };
    assert_eq!(title.chars().count(), TITLE_CHARS + 1);
    assert!(title.ends_with('…'));
}

#[test]
fn fences_and_surrounding_prose_around_the_json_are_stripped() {
    let input = input(RouteScope::Private, "m");
    for text in [
        "```json\n{\"route\":\"list\"}\n```",
        "好的，我的判断是：\n{\"route\":\"list\"}\n以上。",
        "{\"note\":{\"a\":1},\"route\":\"list\"}",
    ] {
        assert_eq!(input.interpret(text), Ok(Route::List), "{text}");
    }
    // 字符串里的花括号不影响配对。
    let route = input
        .interpret(r#"```{"route":"new","description":"输出 {x} 和 }","title":"t"}```"#)
        .unwrap();
    assert_eq!(
        route,
        Route::New {
            description: "输出 {x} 和 }".into(),
            title: "t".into()
        }
    );
}

#[test]
fn the_inputs_are_trimmed_to_the_limits() {
    let tasks: Vec<_> = (0..8)
        .map(|n| task(&format!("f{n}"), CandidateState::Finished))
        .chain([
            task("w", CandidateState::Waiting),
            task("r", CandidateState::Running),
        ])
        .collect();
    let recent: Vec<_> = (0..10)
        .map(|n| RecentMessage {
            speaker: Speaker::Operator,
            text: format!("m{n}"),
        })
        .collect();
    let input = RouterInput::new(
        RouteScope::Private,
        "x",
        tasks,
        recent,
        RouterLimits::default(),
    );
    let ids: Vec<_> = input.tasks.iter().map(|task| task.id.as_str()).collect();
    assert_eq!(ids, ["w", "r", "f0", "f1", "f2", "f3", "f4"]);
    assert_eq!(input.recent.len(), DEFAULT_RECENT_MESSAGES);
    assert_eq!(input.recent[0].text, "m4", "留最近的，旧的在前");
    // 被裁掉的任务不在候选里，指向它就是失败。
    assert_eq!(
        input.interpret(r#"{"route":"continue","task_id":"f7"}"#),
        Err(RouteFailure::UnknownTask("f7".into()))
    );
}
