//! A real child process for protocol tests, with no network or platform account.
use async_trait::async_trait;
use komo_channel::{
    BuiltChannel, ChannelContext, ChannelFactory, ChannelSender, Secrets, SendOutcome,
};
use komo_kernel::{
    protocol::InboundMessage,
    traits::{Channel, ChannelError, DeliverError, Inbound, Shutdown},
    types::{
        chat::{ChannelPeer, ChannelPlatform, Outbound, PeerId},
        ids::RequestKey,
    },
};
use std::{sync::Arc, time::Duration};

struct Fixture;
fn platform() -> ChannelPlatform {
    ChannelPlatform::parse("fixture").unwrap()
}
#[async_trait]
impl ChannelFactory for Fixture {
    fn platform(&self) -> ChannelPlatform {
        platform()
    }
    fn credentials(&self) -> &'static [&'static str] {
        &["FIXTURE_TOKEN"]
    }
    fn build(
        &self,
        _: &ChannelContext,
        secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError> {
        if !secrets.has("FIXTURE_TOKEN") {
            return Err(ChannelError {
                channel: "fixture".into(),
                message: "missing fixture token".into(),
            });
        }
        let fixture = Arc::new(Fixture);
        Ok(Some(BuiltChannel {
            channel: fixture.clone(),
            sender: fixture,
        }))
    }
    async fn probe(&self, _: &ChannelContext, _: &Secrets) -> Result<String, ChannelError> {
        Ok("fixture ready".into())
    }
}
#[async_trait]
impl Channel for Fixture {
    fn name(&self) -> &'static str {
        "fixture"
    }
    async fn serve(
        &self,
        inbound: Arc<dyn Inbound>,
        shutdown: Shutdown,
    ) -> Result<(), ChannelError> {
        inbound
            .handle(InboundMessage {
                peer: ChannelPeer::new(
                    if std::env::args().any(|arg| arg == "--spoof") {
                        ChannelPlatform::Api
                    } else {
                        platform()
                    },
                    "chat",
                ),
                is_private: true,
                sender: PeerId::new("operator"),
                text: "hello".into(),
                context: None,
                same_org: false,
                request_key: RequestKey::new("fixture:event-1"),
            })
            .await
            .map_err(|e| ChannelError {
                channel: "fixture".into(),
                message: e.to_string(),
            })?;
        while !shutdown.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }
}
#[async_trait]
impl ChannelSender for Fixture {
    fn platform(&self) -> ChannelPlatform {
        platform()
    }
    async fn send(&self, _: &ChannelPeer, message: Outbound) -> Result<SendOutcome, DeliverError> {
        if matches!(message, Outbound::Text { ref text } if text == "crash") {
            std::process::exit(23);
        }
        Ok(SendOutcome::Sent)
    }
}
fn main() {
    komo_channel::main(Fixture);
}
