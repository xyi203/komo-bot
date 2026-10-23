//! pi `test/examples/24-child-tasks.ts` 的移植：结账任务拥有四笔支付，`failFast` 等它们。
//! 三个场景：一笔被拒、客户取消、跑到一半"重启"。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use komo_kernel::durable::records::JoinPolicy;
use komo_kernel::durable::{TaskId, TaskState, TaskStatus};
use serde_json::json;

use super::super::{Engine, Ownership, TaskOptions};
use super::support::*;

#[derive(Clone, Default)]
struct Bank {
    charged: Arc<Mutex<HashSet<String>>>,
    log: Log,
}

fn payment(env: &Env, bank: &Bank) -> Arc<Kind> {
    let clock = env.clock.clone();
    let (charge_bank, abort_bank) = (bank.clone(), bank.clone());
    kind("example.payment")
        .initial(move |_| json!({"phase": "charge", "at": clock.now_ms() + 100}))
        .phase("charge", move |task, rt| {
            let bank = charge_bank.clone();
            async move {
                let card = task.input()["card"].as_str().unwrap().to_owned();
                bank.log.push(format!("charge {card}"));
                if card.starts_with("expired") {
                    rt.commit_next(failed(&format!("{card} declined"))).await?;
                    return Ok(());
                }
                bank.charged.lock().unwrap().insert(card.clone());
                rt.sleep_until(task.checkpoint["at"].as_i64().unwrap())
                    .await?;
                rt.commit_next(done(json!({"card": card}))).await?;
                Ok(())
            }
        })
        // 每笔支付在被取消时撤销自己的效果。
        .on_abort(move |task, rt| {
            let bank = abort_bank.clone();
            async move {
                let card = task.input()["card"].as_str().unwrap().to_owned();
                let refunded = bank.charged.lock().unwrap().remove(&card);
                bank.log.push(format!(
                    "payment {card} aborted{}",
                    if refunded { ", refunded" } else { "" }
                ));
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build()
}

fn checkout(payment: Arc<Kind>, bank: &Bank) -> Arc<Kind> {
    let (decide_bank, abort_bank) = (bank.clone(), bank.clone());
    kind("example.checkout")
        .phase("pay", move |task, rt| {
            let payment = payment.clone();
            async move {
                let cards: Vec<String> = task.input()["cards"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|card| card.as_str().unwrap().to_owned())
                    .collect();
                let owner = task.id();
                rt.commit(move |tx, _current| {
                    Box::pin(async move {
                        let mut payments: Vec<TaskId> = Vec::new();
                        for card in cards {
                            payments.push(
                                tx.create_task(
                                    &*payment,
                                    json!({"card": card}),
                                    Ownership::Task(owner),
                                    TaskOptions::default(),
                                )
                                .await?,
                            );
                        }
                        Ok(Some(komo_kernel::durable::task::NextTaskState::Waiting {
                            checkpoint: json!({"phase": "decide", "payments": payments}),
                            on: payments,
                            policy: JoinPolicy::FailFast,
                        }))
                    })
                })
                .await?;
                Ok(())
            }
        })
        .phase("decide", move |task, rt| {
            let bank = decide_bank.clone();
            async move {
                let payments: Vec<TaskId> =
                    serde_json::from_value(task.checkpoint["payments"].clone()).unwrap();
                let outcomes = rt.outcomes(&payments).await?;
                let statuses: Vec<&str> = outcomes.iter().map(status).collect();
                bank.log.push(format!("payments: {}", statuses.join(", ")));
                let paid = outcomes.iter().all(|o| status(o) == "completed");
                rt.commit_next(if paid {
                    done(json!("order placed"))
                } else {
                    failed("payment failed")
                })
                .await?;
                Ok(())
            }
        })
        // 只在每笔支付都结束之后才跑，所以退款已经发生。
        .on_abort(move |_task, rt| {
            let bank = abort_bank.clone();
            async move {
                bank.log.push("checkout aborted");
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .initial(|_| json!({"phase": "pay"}))
        .build()
}

async fn place(engine: &Engine, checkout: &Arc<Kind>, cards: &[&str]) -> TaskId {
    spawn(engine, checkout, json!({"cards": cards})).await
}

fn setup() -> (Env, Bank, Arc<Kind>) {
    let env = Env::new();
    let bank = Bank::default();
    let payment = payment(&env, &bank);
    let checkout = checkout(payment.clone(), &bank);
    env.install(&payment);
    env.install(&checkout);
    (env, bank, checkout)
}

/// 24-child-tasks："One card is declined"：第一笔被拒的支付让 failFast 给其余三笔打 abort 标记，
/// 它们各自退款；结账在四笔都 terminal 之后读到 `[aborted, failed, aborted, aborted]`。
#[tokio::test(start_paused = true)]
async fn one_declined_payment_aborts_the_other_three_and_fails_the_checkout() {
    let (env, bank, checkout) = setup();
    let engine = env.open_running().await;
    let id = place(
        &engine,
        &checkout,
        &["visa-1", "expired-2", "visa-3", "visa-4"],
    )
    .await;
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "failed");
    let lines = bank.log.lines();
    assert!(
        lines.contains(&"payments: aborted, failed, aborted, aborted".to_owned()),
        "{lines:?}"
    );
    for card in ["visa-1", "visa-3", "visa-4"] {
        assert!(lines.contains(&format!("payment {card} aborted, refunded")));
    }
    assert!(
        bank.charged.lock().unwrap().is_empty(),
        "everything was refunded"
    );
    engine.close().await.unwrap();
}

/// 24-child-tasks："The customer cancels"：取消结账，支付先在各自的 abort handler 里退款，
/// 然后才轮到结账自己的 abort handler（自底向上）。
#[tokio::test(start_paused = true)]
async fn cancelling_the_checkout_aborts_the_payments_first_then_the_checkout() {
    let (env, bank, checkout) = setup();
    let engine = env.open_running().await;
    let id = place(
        &engine,
        &checkout,
        &["visa-5", "visa-6", "visa-7", "visa-8"],
    )
    .await;
    until(|| bank.charged.lock().unwrap().len() == 4).await;
    engine.abort_task(id).await.unwrap();
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "aborted");
    let lines: Vec<String> = bank
        .log
        .lines()
        .into_iter()
        .filter(|line| line.contains("aborted"))
        .collect();
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(
        lines.last().unwrap(),
        "checkout aborted",
        "bottom-up: {lines:?}"
    );
    assert!(bank.charged.lock().unwrap().is_empty());
    engine.close().await.unwrap();
}

/// 24-child-tasks："The process stops while the payments run, and a new one continues"：
/// 关掉引擎、在同一个存储上重开；`running` 的支付回到 `pending` 从 checkpoint 继续，
/// 结账一直在 `waiting`。
#[tokio::test(start_paused = true)]
async fn payments_interrupted_by_a_restart_continue_and_the_checkout_completes() {
    let (env, bank, checkout) = setup();
    let engine = env.open_running().await;
    let id = place(
        &engine,
        &checkout,
        &["visa-9", "visa-10", "visa-11", "visa-12"],
    )
    .await;
    until(|| bank.charged.lock().unwrap().len() == 4).await;
    engine.close().await.unwrap();
    // close 不写任何结局：结账在等，支付停在 running。
    let before = record(&env, id).await;
    assert!(
        matches!(before.state, TaskState::Waiting { .. }),
        "{before:?}"
    );

    let engine = env.open().await;
    let live = engine.inspect();
    assert_eq!(live.len(), 5);
    for task in &live {
        match task.record.owner {
            None => assert_eq!(task.record.state.status(), TaskStatus::Waiting),
            Some(_) => assert_eq!(
                task.record.state.status(),
                TaskStatus::Pending,
                "reconciliation puts running tasks back to pending"
            ),
        }
    }
    engine.resume();
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "completed");
    assert_eq!(
        bank.log.count("charge visa-9"),
        2,
        "the payment's phase ran again after restart"
    );
    engine.close().await.unwrap();
}
