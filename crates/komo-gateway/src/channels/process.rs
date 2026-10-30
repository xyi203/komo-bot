//! The Gateway knows the channel protocol, never a platform SDK.
use super::{BuiltChannel, ChannelFactory, ChannelSender, SendOutcome};
use async_trait::async_trait;
use komo_channel::{
    ChannelContext, Descriptor, Initialize, PROTOCOL_VERSION, SendRequest, rpc::Rpc,
};
use komo_kernel::{
    protocol::{InboundMessage, config::ConfigSnapshot},
    traits::{Channel, ChannelError, DeliverError, Inbound, Shutdown},
    types::chat::{ChannelPeer, ChannelPlatform, Outbound},
};
use komo_runtime::config::Secrets;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    sync::{Mutex, OnceCell, mpsc},
};

pub fn factories(snapshot: &ConfigSnapshot) -> Vec<Arc<dyn ChannelFactory>> {
    snapshot
        .channels
        .platforms()
        .into_iter()
        .map(|platform| Arc::new(ProcessFactory(platform)) as Arc<dyn ChannelFactory>)
        .collect()
}

/// Official plugins live beside komo. No PATH search for an accidentally shadowed binary.
fn executable(platform: &ChannelPlatform) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let directory = exe.parent().ok_or("komo 可执行文件没有父目录")?;
    let name = format!("komo-channel-{platform}");
    let bundled = directory
        .join("komo-plugins")
        .join(env!("CARGO_PKG_VERSION"))
        .join(&name);
    if bundled.exists() {
        Ok(bundled)
    } else {
        Ok(directory.join(name))
    }
}

pub async fn login(home: &Path, platform: ChannelPlatform) -> Result<String, String> {
    let (settings, secrets) =
        komo_runtime::config::channel_login_config(home, &platform).map_err(|e| e.to_string())?;
    let plugin = settings.as_ref();
    let path = match plugin {
        Some(p) => p.command.clone(),
        None => executable(&platform)?,
    };
    let mut command = Command::new(path);
    command.env_clear();
    for name in [
        "PATH",
        "HOME",
        "TMPDIR",
        "LANG",
        "TZ",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut names = komo_runtime::config::channel_credentials(platform.clone())
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    if let Some(plugin) = plugin {
        names.extend(plugin.env.iter().cloned());
    }
    for name in names {
        if let Some(value) = secrets.get(&name) {
            command.env(name, value);
        }
    }
    if let Some(plugin) = plugin {
        command.args(&plugin.args);
    }
    let status = command
        .arg("login")
        .arg(home)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|e| format!("启动渠道登录失败：{e}"))?;
    if status.success() {
        Ok("渠道登录成功".into())
    } else {
        Err(format!("渠道登录退出：{status}"))
    }
}

pub(super) struct ProcessFactory(ChannelPlatform);
pub(crate) fn process_factory(platform: ChannelPlatform) -> Arc<dyn ChannelFactory> {
    Arc::new(ProcessFactory(platform))
}
#[async_trait]
impl ChannelFactory for ProcessFactory {
    fn platform(&self) -> ChannelPlatform {
        self.0.clone()
    }
    fn build(
        &self,
        snapshot: &ConfigSnapshot,
        secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError> {
        let Some(config) = snapshot.channels.get(self.0.clone()).filter(|c| c.enabled) else {
            return Ok(None);
        };
        let channel = Arc::new(ProcessChannel {
            platform: self.0.clone(),
            context: ChannelContext {
                config: config.clone(),
                data_dir: snapshot.start_only.data_dir.clone(),
                workspaces_dir: snapshot.paths.workspaces_dir.clone(),
            },
            secrets: secrets.clone(),
            connection: OnceCell::new(),
        });
        Ok(Some(BuiltChannel {
            channel: channel.clone(),
            sender: channel,
        }))
    }
    async fn probe(
        &self,
        snapshot: &ConfigSnapshot,
        secrets: &Secrets,
    ) -> Result<String, ChannelError> {
        let Some(built) = self.build(snapshot, secrets)? else {
            return Err(error(self.0.clone(), "渠道未启用"));
        };
        // A probe must not start a platform consumer.
        let channel = ProcessChannel {
            platform: self.0.clone(),
            context: ChannelContext {
                config: snapshot.channels.get(self.0.clone()).unwrap().clone(),
                data_dir: snapshot.start_only.data_dir.clone(),
                workspaces_dir: snapshot.paths.workspaces_dir.clone(),
            },
            secrets: secrets.clone(),
            connection: OnceCell::new(),
        };
        drop(built);
        let connection = channel
            .connect(false)
            .await
            .map_err(|e| error(self.0.clone(), e))?;
        connection
            .rpc
            .call("probe", &())
            .await
            .map_err(|e| error(self.0.clone(), e))
    }
}
struct Connection {
    rpc: Rpc,
    requests: Mutex<mpsc::Receiver<komo_channel::rpc::Frame>>,
    child: Mutex<Child>,
}
struct ProcessChannel {
    platform: ChannelPlatform,
    context: ChannelContext,
    secrets: Secrets,
    connection: OnceCell<Connection>,
}

impl ProcessChannel {
    async fn connect(&self, start: bool) -> Result<Connection, String> {
        let path = self
            .context
            .config
            .plugin
            .as_ref()
            .map(|p| p.command.clone())
            .map(Ok)
            .unwrap_or_else(|| executable(&self.platform))?;
        let mut command = Command::new(&path);
        if let Some(plugin) = &self.context.config.plugin {
            command.args(&plugin.args);
        }
        command.env_clear();
        for key in [
            "PATH",
            "HOME",
            "TMPDIR",
            "LANG",
            "TZ",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("启动 {} 失败：{e}；请安装同版渠道插件", path.display()))?;
        let (rpc, requests) =
            Rpc::connect(child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let descriptor: Descriptor = rpc.call("describe", &()).await?;
        if descriptor.channel != self.platform || descriptor.version != PROTOCOL_VERSION {
            return Err("渠道插件标识或协议版本不匹配".into());
        }
        if matches!(self.platform, ChannelPlatform::Custom(_)) {
            let allowed = self
                .context
                .config
                .plugin
                .as_ref()
                .map(|p| p.env.as_slice())
                .unwrap_or_default();
            if descriptor
                .credentials
                .iter()
                .any(|name| !allowed.contains(name))
            {
                return Err("插件请求了未在 plugin.env 声明的凭证".into());
            }
        }
        let secrets =
            komo_channel::Secrets::from_pairs(descriptor.credentials.iter().filter_map(|name| {
                self.secrets
                    .get(name)
                    .map(|value| (name.clone(), value.to_owned()))
            }));
        let initialized: Descriptor = rpc
            .call(
                "initialize",
                &Initialize {
                    version: PROTOCOL_VERSION,
                    channel: self.platform.clone(),
                    context: self.context.clone(),
                    secrets,
                },
            )
            .await?;
        if initialized.channel != self.platform || initialized.version != PROTOCOL_VERSION {
            return Err("渠道握手失败".into());
        }
        if start {
            rpc.call::<_, Value>("start", &()).await?;
        }
        Ok(Connection {
            rpc,
            requests: Mutex::new(requests),
            child: Mutex::new(child),
        })
    }
    async fn connection(&self) -> Result<&Connection, String> {
        self.connection.get_or_try_init(|| self.connect(true)).await
    }
}
#[async_trait]
impl ChannelSender for ProcessChannel {
    fn platform(&self) -> ChannelPlatform {
        self.platform.clone()
    }
    async fn send(
        &self,
        peer: &ChannelPeer,
        outbound: Outbound,
    ) -> Result<SendOutcome, DeliverError> {
        self.send_delivery("", peer, outbound).await
    }
    async fn send_delivery(
        &self,
        id: &str,
        peer: &ChannelPeer,
        outbound: Outbound,
    ) -> Result<SendOutcome, DeliverError> {
        if peer.platform != self.platform {
            return Err(DeliverError::Rejected("渠道发送目标不匹配".into()));
        }
        let connection = self.connection().await.map_err(DeliverError::Other)?;
        connection
            .rpc
            .call(
                "send",
                &SendRequest {
                    delivery_id: id.into(),
                    peer: peer.clone(),
                    outbound,
                },
            )
            .await
            .map_err(DeliverError::Other)
    }
}
#[async_trait]
impl Channel for ProcessChannel {
    fn name(&self) -> &'static str {
        "process"
    }
    async fn serve(
        &self,
        inbound: Arc<dyn Inbound>,
        shutdown: Shutdown,
    ) -> Result<(), ChannelError> {
        let connection = self
            .connection()
            .await
            .map_err(|e| error(self.platform.clone(), e))?;
        let mut requests = connection.requests.lock().await;
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { return Err(error(self.platform.clone(),"插件连接已关闭")); };
                    let result = match request.method.as_deref() {
                        Some("inbound") => {
                            match serde_json::from_value::<InboundMessage>(request.payload) {
                                Ok(message) if message.peer.platform == self.platform && message.request_key.as_str().starts_with(&format!("{}:",self.platform)) => {
                                    inbound.handle(message).await.map_err(|e| e.to_string()).and_then(|ack| serde_json::to_value(ack).map_err(|e| e.to_string()))
                                }
                                Ok(_) => Err("插件不能提交其他渠道的消息或请求键".into()),
                                Err(e) => Err(e.to_string()),
                            }
                        }
                        Some("alert") => {
                            match serde_json::from_value::<String>(request.payload) {
                                Ok(text) => inbound.alert(text).await.map(|_| Value::Null).map_err(|e| e.to_string()),
                                Err(e) => Err(e.to_string()),
                            }
                        }
                        _ => Err("不支持的插件宿主请求".into()),
                    };
                    connection.rpc.reply(request.id,result).await.map_err(|e| error(self.platform.clone(),e))?;
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if shutdown.is_cancelled() { break; }
                }
            }
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            connection.rpc.call::<_, Value>("shutdown", &()),
        )
        .await;
        let mut child = connection.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
        Ok(())
    }
}
fn error(platform: ChannelPlatform, message: impl Into<String>) -> ChannelError {
    ChannelError {
        channel: platform.to_string(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::{
        protocol::{
            InboundAck,
            config::{ChannelConfig, ChannelPlugin},
        },
        traits::GatewayError,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Inbox {
        received: tokio::sync::Notify,
        release: tokio::sync::Notify,
        count: AtomicUsize,
    }
    #[async_trait]
    impl Inbound for Inbox {
        async fn handle(&self, _: InboundMessage) -> Result<InboundAck, GatewayError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.received.notify_one();
            self.release.notified().await;
            Ok(InboundAck::Ignored)
        }
    }
    fn fixture(spoof: bool) -> (BuiltChannel, Arc<Inbox>) {
        let mut snapshot = crate::service::test_support::sample_snapshot();
        let directory = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let command = directory.join("komo-channel-fixture");
        assert!(
            command.is_file(),
            "cargo test --workspace builds the protocol fixture"
        );
        snapshot.channels.extra.insert(
            "fixture".into(),
            ChannelConfig {
                enabled: true,
                plugin: Some(ChannelPlugin {
                    command,
                    args: if spoof {
                        vec!["--spoof".into()]
                    } else {
                        vec![]
                    },
                    env: vec!["FIXTURE_TOKEN".into()],
                }),
                ..Default::default()
            },
        );
        let factory = ProcessFactory(ChannelPlatform::parse("fixture").unwrap());
        (
            factory
                .build(
                    &snapshot,
                    &Secrets::from_pairs([("FIXTURE_TOKEN", "secret")]),
                )
                .unwrap()
                .unwrap(),
            Arc::new(Inbox::default()),
        )
    }
    #[tokio::test]
    async fn an_external_channel_uses_the_host_inbound_and_sender_without_core_registration() {
        let (built, inbox) = fixture(false);
        let stop = Shutdown::new();
        let task = {
            let channel = built.channel.clone();
            let inbox = inbox.clone();
            let stop = stop.clone();
            tokio::spawn(async move { channel.serve(inbox, stop).await })
        };
        tokio::time::timeout(Duration::from_secs(5), inbox.received.notified())
            .await
            .unwrap();
        let outcome = built
            .sender
            .send_delivery(
                "stable-id",
                &ChannelPeer::new(ChannelPlatform::parse("fixture").unwrap(), "chat"),
                Outbound::Text {
                    text: "response".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome, SendOutcome::Sent);
        inbox.release.notify_one();
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(inbox.count.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn a_plugin_cannot_impersonate_the_authenticated_api_entrypoint() {
        let (built, inbox) = fixture(true);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            built.channel.serve(inbox.clone(), Shutdown::new()),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(inbox.count.load(Ordering::SeqCst), 0);
    }
}
