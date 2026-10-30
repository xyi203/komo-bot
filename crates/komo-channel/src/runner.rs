use crate::{rpc::Rpc, *};
use async_trait::async_trait;
use komo_kernel::{
    protocol::{InboundAck, InboundMessage},
    traits::{ChannelError, GatewayError, Inbound, Shutdown},
};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

static ALERTS: OnceLock<std::sync::Mutex<Option<Rpc>>> = OnceLock::new();
struct Alerts;
#[async_trait]
impl AlertSink for Alerts {
    async fn alert(&self, text: String) {
        let rpc = ALERTS
            .get_or_init(Default::default)
            .lock()
            .expect("alerts")
            .clone();
        if let Some(rpc) = rpc {
            let _ = rpc.call::<_, Value>("alert", &text).await;
        }
    }
}
pub fn alerts() -> Arc<dyn AlertSink> {
    Arc::new(Alerts)
}
pub fn login_requested() -> bool {
    std::env::args().nth(1).as_deref() == Some("login")
}
struct Incoming(Rpc);
#[async_trait]
impl Inbound for Incoming {
    async fn handle(&self, msg: InboundMessage) -> Result<InboundAck, GatewayError> {
        self.0
            .call("inbound", &msg)
            .await
            .map_err(GatewayError::Internal)
    }
}

pub fn main(factory: impl ChannelFactory) {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{} {}", factory.platform(), env!("CARGO_PKG_VERSION"));
        return;
    }
    if login_requested() {
        eprintln!("这个渠道使用 .env 凭证，不支持交互登录");
        std::process::exit(2);
    }
    let descriptor = Descriptor {
        version: PROTOCOL_VERSION,
        channel: factory.platform(),
        credentials: factory
            .credentials()
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    if std::env::args().nth(1).as_deref() == Some("--describe") {
        println!(
            "{}",
            serde_json::to_string(&descriptor).expect("descriptor")
        );
        return;
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("channel runtime");
    let result = runtime.block_on(run(Arc::new(factory), descriptor));
    // Tokio stdin owns a blocking read which cannot be cancelled. Waiting for runtime
    // Drop would deadlock shutdown while the parent still holds the pipe open.
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run(factory: Arc<dyn ChannelFactory>, descriptor: Descriptor) -> Result<(), String> {
    let (rpc, mut requests) = Rpc::connect(tokio::io::stdin(), tokio::io::stdout());
    *ALERTS.get_or_init(Default::default).lock().expect("alerts") = Some(rpc.clone());
    let shutdown = Shutdown::new();
    let mut sends = tokio::task::JoinSet::new();
    let mut context: Option<Initialize> = None;
    let mut built: Option<Arc<BuiltChannel>> = None;
    let mut serve: Option<tokio::task::JoinHandle<Result<(), ChannelError>>> = None;
    loop {
        let request = tokio::select! {
            request = requests.recv() => match request { Some(r) => r, None => break },
            _ = sends.join_next(), if !sends.is_empty() => continue,
            result = async { match serve.as_mut() { Some(task) => Some(task.await), None => std::future::pending().await } } => {
                return Err(format!("渠道接收循环已退出：{result:?}"));
            }
        };
        let method = request.method.as_deref().unwrap_or_default();
        let result: Result<Value, String> = match method {
            "describe" => serde_json::to_value(&descriptor).map_err(|e| e.to_string()),
            "initialize" if context.is_none() => (|| {
                let init: Initialize =
                    serde_json::from_value(request.payload).map_err(|e| e.to_string())?;
                if init.version != PROTOCOL_VERSION || init.channel != descriptor.channel {
                    return Err("渠道标识或协议版本不匹配".into());
                }
                context = Some(init);
                serde_json::to_value(&descriptor).map_err(|e| e.to_string())
            })(),
            "start" if context.is_some() && built.is_none() => {
                let init = context.as_ref().unwrap();
                match factory.build(&init.context, &init.secrets) {
                    Ok(Some(channel)) => {
                        let channel = Arc::new(channel);
                        let running = channel.clone();
                        let inbound = Arc::new(Incoming(rpc.clone()));
                        let stop = shutdown.clone();
                        serve = Some(tokio::spawn(async move {
                            running.channel.serve(inbound, stop).await
                        }));
                        built = Some(channel);
                        Ok(json!(null))
                    }
                    Ok(None) => Err("渠道没有就绪，请检查凭证或登录".into()),
                    Err(e) => Err(e.to_string()),
                }
            }
            "send" if built.is_some() => {
                let channel = built.as_ref().unwrap().clone();
                let reply = rpc.clone();
                let id = request.id;
                sends.spawn(async move {
                    let result = async {
                        let send: SendRequest =
                            serde_json::from_value(request.payload).map_err(|e| e.to_string())?;
                        let outcome = channel
                            .sender
                            .send_delivery(&send.delivery_id, &send.peer, send.outbound)
                            .await
                            .map_err(|e| e.to_string())?;
                        serde_json::to_value(outcome).map_err(|e| e.to_string())
                    }
                    .await;
                    let _ = reply.reply(id, result).await;
                });
                continue;
            }
            "probe" if context.is_some() => {
                let init = context.as_ref().unwrap();
                factory
                    .probe(&init.context, &init.secrets)
                    .await
                    .map(|s| json!(s))
                    .map_err(|e| e.to_string())
            }
            "shutdown" => {
                shutdown.cancel();
                rpc.reply(request.id, Ok(Value::Null)).await?;
                break;
            }
            _ => Err(format!("当前阶段不支持渠道请求 {method}")),
        };
        rpc.reply(request.id, result).await?;
    }
    shutdown.cancel();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        if let Some(task) = serve.as_mut() {
            let _ = task.await;
        }
        while sends.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        if let Some(task) = serve {
            task.abort();
        }
        sends.abort_all();
    }
    Ok(())
}
