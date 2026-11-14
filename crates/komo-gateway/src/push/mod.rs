//! 浏览器推送的服务端（`docs/redesign.md` §1.8）：存订阅、管 VAPID 密钥、往订阅的设备发系统通知。
//!
//! - **订阅**存在 `aux.db` 的 `push_subscriptions`（[`TursoPushRepo`]）；同一个 endpoint 再订阅是更新。
//! - **VAPID 密钥对**首次需要时生成，落在 `<数据目录>/runtime/vapid.json`（0600，PKCS#8 的 base64url）。
//!   与本机令牌 `runtime/gateway.json` 一样放在 `runtime/` 下——`write` / `edit` 工具本来就拒绝改它；换一个
//!   数据目录等于换一对密钥，旧订阅会在第一次发送时被推送服务回 4xx 而清掉，网页重新订阅即可。
//! - **发送**按 RFC 8291（[`webpush::encrypt`]）+ RFC 8292（[`webpush::Vapid`]）。后台发、不等结果：推送失败
//!   只记日志，不影响把话投进对话。推送服务回 404 / 410 说明订阅已失效，删掉。
//! - **什么时候推**由调用方决定（[`PushService::notify`]）：停住的运行在对话里问你、定时结果按 `notify`
//!   投进对话、Komo 在对话里主动说的话。没有订阅、总开关关了（`[push] enabled = false`）、没有 `subject`
//!   可用时什么都不做。

pub mod webpush;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use komo_config::ConfigHolder;
use komo_kernel::traits::{Clock, StoreError};
use komo_store::{Db, NewPushSubscription, PushSubscription, TursoPushRepo};
use serde::Serialize;
use tokio::sync::OnceCell;

use webpush::{Vapid, WebPushError};

/// 一次发送最多等多久。
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// 推送服务缓存这条消息多久（设备离线时）：一天，过了就不再有意义。
const TTL_SECS: u32 = 24 * 60 * 60;
/// VAPID JWT 的有效期（RFC 8292 要求不超过 24 小时）。
const JWT_TTL_SECS: i64 = 12 * 60 * 60;
/// 通知标题 / 正文最多这么多字符（系统通知本来就只显示开头）。
const TITLE_CHARS: usize = 60;
const BODY_CHARS: usize = 140;
const ENDPOINT_MAX: usize = 2048;

/// 推送失败的几种。
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Crypto(#[from] WebPushError),
    #[error("VAPID 密钥文件读写失败：{0}")]
    Io(String),
}

/// 发给 service worker 的负载（`packages/protocol/src/push.ts` 的 `PushPayload`）。
#[derive(Debug, Clone, Serialize)]
pub struct PushMessage {
    pub title: String,
    pub body: String,
    /// 站内路径：主对话 `/`，支线 `/chats/<会话 id>`。
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

pub struct PushService {
    repo: TursoPushRepo,
    config: Arc<ConfigHolder>,
    clock: Arc<dyn Clock>,
    key_path: PathBuf,
    vapid: OnceCell<Arc<Vapid>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for PushService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushService").finish_non_exhaustive()
    }
}

/// 一次发送的结局。
enum Outcome {
    Sent,
    /// 订阅已失效（404 / 410）：删掉。
    Gone,
    Failed(String),
}

impl PushService {
    pub fn new(
        home: &std::path::Path,
        db: Db,
        config: Arc<ConfigHolder>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        PushService {
            repo: TursoPushRepo::new(db),
            config,
            clock,
            key_path: home.join("runtime").join("vapid.json"),
            vapid: OnceCell::new(),
            http: reqwest::Client::builder()
                .timeout(SEND_TIMEOUT)
                // 推送服务的地址是浏览器给的：不跟着跳转，免得被引到别处。
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
        }
    }

    async fn vapid(&self) -> Result<&Arc<Vapid>, PushError> {
        self.vapid
            .get_or_try_init(|| async {
                let path = self.key_path.clone();
                tokio::task::spawn_blocking(move || load_or_create(&path))
                    .await
                    .map_err(|error| PushError::Io(error.to_string()))?
                    .map(Arc::new)
            })
            .await
    }

    /// `GET /v1/push/key`。
    pub async fn public_key(&self) -> Result<String, PushError> {
        Ok(self.vapid().await?.public_key().to_string())
    }

    /// `POST /v1/push/subscriptions`：返回订阅的 id。
    pub async fn subscribe(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        user_agent: Option<&str>,
    ) -> Result<String, PushError> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| PushError::Invalid("endpoint 不是合法的地址".into()))?;
        if endpoint.len() > ENDPOINT_MAX || url.scheme() != "https" || url.host_str().is_none() {
            return Err(PushError::Invalid(
                "endpoint 必须是 https 地址（不超过 2048 字符）".into(),
            ));
        }
        // 提前验：解不出来的密钥存了也发不了。
        let decoded = |value: &str| URL_SAFE_NO_PAD.decode(value.trim().trim_end_matches('='));
        if decoded(p256dh)
            .map(|key| key.len() != 65 || key[0] != 0x04)
            .unwrap_or(true)
        {
            return Err(PushError::Invalid(
                "keys.p256dh 不是未压缩的 P-256 公钥".into(),
            ));
        }
        if decoded(auth).map(|key| key.len() != 16).unwrap_or(true) {
            return Err(PushError::Invalid(
                "keys.auth 应是 16 字节的 base64url".into(),
            ));
        }
        let user_agent = user_agent
            .map(|ua| clip(ua, 300))
            .filter(|ua| !ua.trim().is_empty());
        Ok(self
            .repo
            .upsert(NewPushSubscription {
                endpoint: endpoint.to_string(),
                p256dh: p256dh.trim().to_string(),
                auth: auth.trim().to_string(),
                user_agent,
                now: self.clock.now(),
            })
            .await?)
    }

    /// `DELETE /v1/push/subscriptions`：没有这条也算成功（幂等）。
    pub async fn unsubscribe(&self, endpoint: &str) -> Result<(), PushError> {
        self.repo.remove(endpoint).await?;
        Ok(())
    }

    /// 往所有订阅的设备推一条。**立即返回**，发送在后台；任何失败只记日志。
    pub fn notify(self: &Arc<Self>, message: PushMessage) {
        let start_only = self.config.current().start_only.clone();
        if !start_only.push.enabled {
            return;
        }
        let Some(subject) = start_only
            .push
            .subject
            .clone()
            .or_else(|| start_only.oidc.as_ref().map(|oidc| oidc.public_url.clone()))
            .filter(|subject| !subject.is_empty())
        else {
            tracing::debug!("没有 [push] subject 也没有 [auth.oidc] public_url，不发推送");
            return;
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = this.deliver(&subject, message).await {
                tracing::warn!(%error, "推送没发出去");
            }
        });
    }

    async fn deliver(&self, subject: &str, message: PushMessage) -> Result<(), PushError> {
        let subscriptions = self.repo.list().await?;
        if subscriptions.is_empty() {
            return Ok(());
        }
        let message = PushMessage {
            title: clip(&message.title, TITLE_CHARS),
            body: clip(&message.body, BODY_CHARS),
            ..message
        };
        let payload =
            serde_json::to_vec(&message).map_err(|error| PushError::Invalid(error.to_string()))?;
        let payload = payload.as_slice();
        let vapid = self.vapid().await?;
        let expires = self.clock.now().unix_timestamp() + JWT_TTL_SECS;
        let sends = subscriptions.iter().map(|sub| async move {
            (
                sub,
                self.send_one(vapid, subject, expires, sub, payload).await,
            )
        });
        for (sub, outcome) in futures_util::future::join_all(sends).await {
            match outcome {
                Outcome::Sent => {}
                Outcome::Gone => {
                    tracing::info!(id = %sub.id, "推送订阅已失效，删掉");
                    if let Err(error) = self.repo.remove(&sub.endpoint).await {
                        tracing::warn!(%error, "失效的推送订阅没删成");
                    }
                }
                Outcome::Failed(reason) => {
                    tracing::warn!(id = %sub.id, %reason, "这台设备的推送没发成");
                }
            }
        }
        Ok(())
    }

    async fn send_one(
        &self,
        vapid: &Vapid,
        subject: &str,
        expires: i64,
        sub: &PushSubscription,
        payload: &[u8],
    ) -> Outcome {
        let Ok(url) = reqwest::Url::parse(&sub.endpoint) else {
            return Outcome::Gone;
        };
        let authorization =
            match vapid.authorization(&url.origin().ascii_serialization(), subject, expires) {
                Ok(value) => value,
                Err(error) => return Outcome::Failed(error.to_string()),
            };
        let body = match webpush::encrypt(payload, &sub.p256dh, &sub.auth) {
            Ok(body) => body,
            // 存下来的密钥解不开：这条订阅永远发不了。
            Err(WebPushError::BadSubscription(_)) => return Outcome::Gone,
            Err(error) => return Outcome::Failed(error.to_string()),
        };
        let response = self
            .http
            .post(url)
            .header("Authorization", authorization)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", TTL_SECS.to_string())
            .header("Urgency", "normal")
            .body(body)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => Outcome::Sent,
            Ok(response) if matches!(response.status().as_u16(), 404 | 410) => Outcome::Gone,
            Ok(response) => Outcome::Failed(format!("推送服务回了 {}", response.status())),
            Err(error) => Outcome::Failed(error.to_string()),
        }
    }
}

fn clip(text: &str, chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(chars).collect();
    cut.push('…');
    cut
}

/// 读 `vapid.json`；没有就生成一对写进去（0600，`create_new`）。
fn load_or_create(path: &std::path::Path) -> Result<Vapid, PushError> {
    use std::io::Write as _;
    let io = |error: std::io::Error| PushError::Io(format!("{}：{error}", path.display()));
    let read = || -> Result<Option<Vapid>, PushError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io(error)),
        };
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
            PushError::Io(format!("{} 不是合法的 JSON：{error}", path.display()))
        })?;
        let pkcs8 = value
            .get("pkcs8")
            .and_then(|field| field.as_str())
            .and_then(|field| URL_SAFE_NO_PAD.decode(field).ok())
            .ok_or_else(|| PushError::Io(format!("{} 里没有可用的 pkcs8", path.display())))?;
        Ok(Some(Vapid::from_pkcs8(&pkcs8)?))
    };
    if let Some(vapid) = read()? {
        return Ok(vapid);
    }
    let (vapid, pkcs8) = Vapid::generate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            let body = serde_json::json!({ "pkcs8": URL_SAFE_NO_PAD.encode(&pkcs8) }).to_string();
            file.write_all(body.as_bytes()).map_err(io)?;
            file.sync_all().map_err(io)?;
            Ok(vapid)
        }
        // 别处刚写好：以落盘的为准。
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read()?.ok_or_else(|| PushError::Io(format!("{} 写到一半", path.display())))
        }
        Err(error) => Err(io(error)),
    }
}
