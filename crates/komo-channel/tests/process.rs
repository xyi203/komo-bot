use komo_channel::{
    Descriptor, Initialize, PROTOCOL_VERSION, Secrets, SendOutcome, SendRequest, rpc::Rpc,
};
use komo_kernel::{
    protocol::{InboundAck, InboundMessage},
    types::chat::{ChannelPeer, ChannelPlatform, Outbound},
};
use serde_json::Value;
use std::{process::Stdio, time::Duration};

async fn connect() -> (
    tokio::process::Child,
    Rpc,
    tokio::sync::mpsc::Receiver<komo_channel::rpc::Frame>,
) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_komo-channel-fixture"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (rpc, incoming) = Rpc::connect(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    (child, rpc, incoming)
}
async fn initialize(rpc: &Rpc) {
    let descriptor: Descriptor = rpc.call("describe", &()).await.unwrap();
    assert_eq!(
        descriptor.channel,
        ChannelPlatform::parse("fixture").unwrap()
    );
    assert_eq!(descriptor.credentials, ["FIXTURE_TOKEN"]);
    let _: Descriptor = rpc
        .call(
            "initialize",
            &Initialize {
                version: PROTOCOL_VERSION,
                channel: descriptor.channel,
                context: komo_channel::test_context(),
                secrets: Secrets::from_pairs([("FIXTURE_TOKEN", "secret")]),
            },
        )
        .await
        .unwrap();
}
#[tokio::test]
async fn a_child_can_send_while_an_inbound_callback_waits_for_its_ack() {
    let (mut child, rpc, mut incoming) = connect().await;
    initialize(&rpc).await;
    let _: Value = rpc.call("start", &()).await.unwrap();
    let callback = tokio::time::timeout(Duration::from_secs(5), incoming.recv())
        .await
        .unwrap()
        .unwrap();
    let message: InboundMessage = serde_json::from_value(callback.payload).unwrap();
    assert_eq!(message.request_key.as_str(), "fixture:event-1");
    let outcome: SendOutcome = rpc
        .call(
            "send",
            &SendRequest {
                delivery_id: "delivery-1".into(),
                peer: message.peer,
                outbound: Outbound::Text {
                    text: "reply".into(),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome, SendOutcome::Sent);
    rpc.reply(
        callback.id,
        Ok(serde_json::to_value(InboundAck::Ignored).unwrap()),
    )
    .await
    .unwrap();
    let _: Value = rpc.call("shutdown", &()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
#[tokio::test]
async fn incompatible_protocol_is_rejected_before_a_consumer_starts() {
    let (_child, rpc, mut incoming) = connect().await;
    let error = rpc
        .call::<_, Descriptor>(
            "initialize",
            &Initialize {
                version: PROTOCOL_VERSION + 1,
                channel: ChannelPlatform::parse("fixture").unwrap(),
                context: komo_channel::test_context(),
                secrets: Secrets::new(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.contains("版本"));
    assert!(incoming.try_recv().is_err());
    assert!(rpc.call::<_, Value>("start", &()).await.is_err());
}
#[tokio::test]
async fn a_child_crash_fails_the_pending_send_without_waiting_for_timeout() {
    let (_child, rpc, mut incoming) = connect().await;
    initialize(&rpc).await;
    let _: Value = rpc.call("start", &()).await.unwrap();
    let callback = incoming.recv().await.unwrap();
    rpc.reply(
        callback.id,
        Ok(serde_json::to_value(InboundAck::Ignored).unwrap()),
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        rpc.call::<_, SendOutcome>(
            "send",
            &SendRequest {
                delivery_id: "delivery-2".into(),
                peer: ChannelPeer::new(ChannelPlatform::parse("fixture").unwrap(), "chat"),
                outbound: Outbound::Text {
                    text: "crash".into(),
                },
            },
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
}
#[tokio::test]
async fn a_cancelled_call_costs_that_call_and_keeps_the_connection() {
    let (_child, rpc, mut incoming) = connect().await;
    initialize(&rpc).await;
    let _: Value = rpc.call("start", &()).await.unwrap();
    let callback = incoming.recv().await.unwrap();
    rpc.reply(
        callback.id,
        Ok(serde_json::to_value(InboundAck::Ignored).unwrap()),
    )
    .await
    .unwrap();
    let send = |text: &str| SendRequest {
        delivery_id: format!("delivery-{text}"),
        peer: ChannelPeer::new(ChannelPlatform::parse("fixture").unwrap(), "chat"),
        outbound: Outbound::Text { text: text.into() },
    };
    // 调用方放弃等待（超时或取消）只丢这一次调用，后面的照常收发
    let slow = send("slow");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            rpc.call::<_, SendOutcome>("send", &slow)
        )
        .await
        .is_err()
    );
    let outcome: SendOutcome = rpc.call("send", &send("after")).await.unwrap();
    assert_eq!(outcome, SendOutcome::Sent);
}
