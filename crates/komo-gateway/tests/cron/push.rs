//! 浏览器推送（`docs/redesign.md` §1.8）在真 Gateway 上：订阅接口、触发点（停住的提问、定时结果）、
//! 重复投递不再推、失效订阅被删、总开关与 `subject`。
//!
//! 推送服务是本机的假 axum 服务。订阅接口只收 https，所以带假服务地址的订阅直接写进 `push_subscriptions`
//! （与接口同一张表）；发送本身不挑 scheme。订阅用的 P-256 密钥是测试里 `ring` 生成的，于是能在假服务
//! 里把收到的负载**真解密**出来（RFC 8291 的接收端）。

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use komo_store::{NewPushSubscription, TursoPushRepo};
use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
use ring::hkdf::{HKDF_SHA256, KeyType, Salt};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey as SigKey};

use crate::confirm::{bump_then, outward_home};
use crate::harness::*;

const SUBJECT: &str = "mailto:ops@example.com";

fn push_section() -> String {
    format!("\n[push]\nsubject = \"{SUBJECT}\"\n")
}

// ---------------------------------------------------------------- 假推送服务

struct Hit {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<Hit>>>);

impl Sink {
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn paths(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|h| h.path.clone())
            .collect()
    }
}

/// `/ok` 回 201，`/gone` 回 410，其余 500。
async fn record(State(sink): State<Sink>, uri: Uri, headers: HeaderMap, body: Bytes) -> StatusCode {
    let path = uri.path().to_string();
    sink.0.lock().unwrap().push(Hit {
        path: path.clone(),
        headers,
        body: body.to_vec(),
    });
    match path.as_str() {
        "/ok" => StatusCode::CREATED,
        "/gone" => StatusCode::GONE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn spawn_push_service() -> (String, Sink) {
    let sink = Sink::default();
    let app = Router::new().fallback(record).with_state(sink.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (origin, sink)
}

// ---------------------------------------------------------------- 浏览器一侧

/// 一台「浏览器」：自己的 P-256 密钥与 auth secret。
struct Browser {
    private: Option<EphemeralPrivateKey>,
    p256dh: String,
    auth_secret: [u8; 16],
}

impl Browser {
    fn new() -> Browser {
        let rng = SystemRandom::new();
        let private = EphemeralPrivateKey::generate(&ECDH_P256, &rng).unwrap();
        let p256dh = B64.encode(private.compute_public_key().unwrap().as_ref());
        let mut auth_secret = [0u8; 16];
        rng.fill(&mut auth_secret).unwrap();
        Browser {
            private: Some(private),
            p256dh,
            auth_secret,
        }
    }

    fn auth(&self) -> String {
        B64.encode(self.auth_secret)
    }

    /// RFC 8291 §4 的接收端：用自己的私钥与请求体头里的发送方公钥还原明文。**只能解一次**（`ring` 的临时
    /// 私钥用一次就消耗了），所以一个 `Browser` 只对应一条消息。
    fn decrypt(&mut self, body: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        assert_eq!(&body[16..20], &4096u32.to_be_bytes(), "rs");
        let id_len = body[20] as usize;
        let as_public = &body[21..21 + id_len];
        let record = &body[21 + id_len..];
        let ua_public = B64.decode(&self.p256dh).unwrap();
        let shared = agree_ephemeral(
            self.private.take().expect("只解一次"),
            &UnparsedPublicKey::new(&ECDH_P256, as_public),
            |secret| secret.to_vec(),
        )
        .unwrap();
        struct Len(usize);
        impl KeyType for Len {
            fn len(&self) -> usize {
                self.0
            }
        }
        let expand = |prk: &ring::hkdf::Prk, info: &[u8], n: usize| {
            let mut out = vec![0u8; n];
            prk.expand(&[info], Len(n)).unwrap().fill(&mut out).unwrap();
            out
        };
        let mut info = b"WebPush: info\0".to_vec();
        info.extend_from_slice(&ua_public);
        info.extend_from_slice(as_public);
        let ikm = expand(
            &Salt::new(HKDF_SHA256, &self.auth_secret).extract(&shared),
            &info,
            32,
        );
        let prk = Salt::new(HKDF_SHA256, salt).extract(&ikm);
        let cek = expand(&prk, b"Content-Encoding: aes128gcm\0", 16);
        let nonce: [u8; 12] = expand(&prk, b"Content-Encoding: nonce\0", 12)
            .try_into()
            .unwrap();
        let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).unwrap());
        let mut buffer = record.to_vec();
        let plain = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut buffer,
            )
            .expect("密文能用接收端的密钥解开");
        let end = plain.iter().rposition(|b| *b == 0x02).expect("结束分隔符");
        plain[..end].to_vec()
    }
}

async fn subscribe_direct(gw: &Gw, endpoint: &str, browser: &Browser) {
    TursoPushRepo::new(gw.state().db.clone())
        .upsert(NewPushSubscription {
            endpoint: endpoint.to_string(),
            p256dh: browser.p256dh.clone(),
            auth: browser.auth(),
            user_agent: None,
            now: time::OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();
}

async fn subscriptions(gw: &Gw) -> Vec<String> {
    TursoPushRepo::new(gw.state().db.clone())
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|sub| sub.endpoint)
        .collect()
}

/// 验 VAPID 头：公钥就是 `GET /v1/push/key` 给网页的那把，JWT 的 aud 是推送服务的源，sub 是配置里的。
async fn assert_vapid(gw: &Gw, hit: &Hit, origin: &str) {
    let (status, key) = gw.get_json("/v1/push/key").await;
    assert_eq!(status, 200, "{key}");
    let public = key["public_key"].as_str().unwrap();
    let header = hit.headers["authorization"].to_str().unwrap();
    let rest = header.strip_prefix("vapid t=").expect("vapid 头");
    let (jwt, k) = rest.split_once(", k=").unwrap();
    assert_eq!(k, public);
    let (signing, signature) = jwt.rsplit_once('.').unwrap();
    SigKey::new(&ECDSA_P256_SHA256_FIXED, B64.decode(public).unwrap())
        .verify(signing.as_bytes(), &B64.decode(signature).unwrap())
        .expect("签名能用公布的公钥验过");
    let claims: serde_json::Value =
        serde_json::from_slice(&B64.decode(signing.split('.').nth(1).unwrap()).unwrap()).unwrap();
    assert_eq!(claims["aud"], origin);
    assert_eq!(claims["sub"], SUBJECT);
    let exp = claims["exp"].as_i64().unwrap();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    assert!(
        exp > now && exp <= now + 24 * 3600,
        "exp 在 24 小时之内：{exp}"
    );
    assert_eq!(hit.headers["content-encoding"], "aes128gcm");
    assert_eq!(hit.headers["ttl"], "86400");
}

fn daily(notify: &str) -> serde_json::Value {
    serde_json::json!({
        "name": "morning-summary",
        "schedule": "0 9 * * *",
        "timezone": "Asia/Shanghai",
        "prompt": "整理今天的技术动态",
        "notify": notify,
    })
}

async fn fire_and_finish(gw: &Gw) {
    let job = gw.add_job(daily("always")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
}

// ---------------------------------------------------------------- 测试

/// 停住的提问（定时 Run 要调对外工具）推一次：负载能解开、VAPID 对得上；重启后补投判重，不再推第二次。
#[tokio::test]
async fn a_stopped_ask_pushes_once_and_a_replay_after_restart_does_not_push_again() {
    let (home, _counter) = outward_home(&push_section());
    let (origin, sink) = spawn_push_service().await;
    let gw = home.start(bump_then("加好了")).await;
    let mut browser = Browser::new();
    subscribe_direct(&gw, &format!("{origin}/ok"), &browser).await;

    let job = gw
        .add_job(serde_json::json!({
            "name": "bump-daily", "schedule": "0 9 * * *", "timezone": "Asia/Shanghai",
            "prompt": "加一",
        }))
        .await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_intervention(&run).await;
    eventually("推送到了", || sink.count() == 1).await;

    {
        let hits = sink.0.lock().unwrap();
        let hit = &hits[0];
        assert_eq!(hit.path, "/ok");
        assert_vapid(&gw, hit, &origin).await;
        let payload: serde_json::Value =
            serde_json::from_slice(&browser.decrypt(&hit.body)).expect("负载是 JSON");
        assert_eq!(payload["title"], "Komo 在等你");
        assert_eq!(payload["url"], "/", "没绑支线：回主对话");
        assert!(
            payload["body"]
                .as_str()
                .unwrap()
                .contains("mcp__demo__bump"),
            "{payload}"
        );
        assert!(
            payload["tag"]
                .as_str()
                .unwrap()
                .starts_with("intervention:")
        );
    }

    // 重启：启动时把没答复的介入回放一遍，那句话已经在了（判重），不再推。
    gw.stop().await;
    let gw = home.start(bump_then("不会用到")).await;
    assert_eq!(subscriptions(&gw).await.len(), 1, "订阅落盘，重启后还在");
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(sink.count(), 1, "重复投递不再推：{:?}", sink.paths());
    gw.stop().await;
}

/// 定时结果推一次；推送服务回 410 的订阅被删掉，回 200 的留着；订阅整体失败不影响把话投进对话。
/// 操作者自己发起的那一轮不推。
#[tokio::test]
async fn a_cron_result_pushes_a_dead_subscription_is_dropped_and_the_operators_own_turn_is_quiet() {
    let home = Home::with_config(&config_toml(&push_section()));
    let (origin, sink) = spawn_push_service().await;
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "整理好了")]))
        .await;
    let (alive, dead) = (Browser::new(), Browser::new());
    subscribe_direct(&gw, &format!("{origin}/ok"), &alive).await;
    subscribe_direct(&gw, &format!("{origin}/gone"), &dead).await;

    fire_and_finish(&gw).await;
    eventually("两台设备都收到", || sink.count() == 2).await;
    for _ in 0..100 {
        if subscriptions(&gw).await.len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(subscriptions(&gw).await, [format!("{origin}/ok")]);
    {
        let mut alive = alive;
        let hits = sink.0.lock().unwrap();
        let hit = hits.iter().find(|h| h.path == "/ok").unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&alive.decrypt(&hit.body)).unwrap();
        assert_eq!(payload["title"], "Komo 的定时任务");
        assert!(payload["body"].as_str().unwrap().contains("整理好了"));
    }

    // 操作者在对话里发消息，那一轮的回复他正看着：不推。
    let session = gw.open_session().await;
    let submitted = gw.submit(&session, "rk-push-1", "你好").await;
    gw.wait_state(&submitted.run, |s| s.is_terminal(), "终态")
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(sink.count(), 2, "{:?}", sink.paths());
    gw.stop().await;
}

/// `[push] enabled = false`、没有 subject（也没有 OIDC 公开地址）：什么都不发，接口照常。
#[tokio::test]
async fn nothing_is_sent_when_push_is_off_or_there_is_no_subject() {
    for config in [
        config_toml(&format!("{}\nenabled = false\n", push_section())),
        config_toml(""),
    ] {
        let home = Home::with_config(&config);
        let (origin, sink) = spawn_push_service().await;
        let gw = home
            .start(FakeLlm::always(vec![text_round(1, "整理好了")]))
            .await;
        subscribe_direct(&gw, &format!("{origin}/ok"), &Browser::new()).await;
        fire_and_finish(&gw).await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert_eq!(sink.count(), 0, "{config}");
        let (status, key) = gw.get_json("/v1/push/key").await;
        assert_eq!(status, 200, "总开关只管发送，不管取公钥：{key}");
        gw.stop().await;
    }
}

/// 订阅接口：非 https 与坏密钥被拒；同一 endpoint 再订阅是更新（同一个 id）；退订幂等；VAPID 公钥落盘、重启不变。
#[tokio::test]
async fn the_subscription_api_upserts_refuses_bad_input_and_unsubscribes_idempotently() {
    let home = Home::with_config(&config_toml(&push_section()));
    let gw = home.start(FakeLlm::finisher("好")).await;
    let browser = Browser::new();
    let endpoint = "https://push.example.com/send/abc";
    let body = |endpoint: &str, p256dh: &str, auth: &str| {
        serde_json::json!({
            "endpoint": endpoint,
            "keys": { "p256dh": p256dh, "auth": auth },
            "user_agent": "Mozilla/5.0 Chrome/130",
        })
    };
    let url = "/v1/push/subscriptions";

    for bad in [
        body(
            "http://push.example.com/x",
            &browser.p256dh,
            &browser.auth(),
        ),
        body("not a url", &browser.p256dh, &browser.auth()),
        body(endpoint, "AAAA", &browser.auth()),
        body(endpoint, &browser.p256dh, "AAAA"),
        body(
            &format!("https://push.example.com/{}", "x".repeat(2100)),
            &browser.p256dh,
            &browser.auth(),
        ),
    ] {
        let (status, text) = gw.post_json(url, bad.clone()).await;
        assert_eq!(status, 400, "{bad}：{text}");
    }
    assert!(subscriptions(&gw).await.is_empty(), "拒绝的不落库");

    let (status, first) = gw
        .post_json(url, body(endpoint, &browser.p256dh, &browser.auth()))
        .await;
    assert_eq!(status, 200, "{first}");
    let other = Browser::new();
    let (status, second) = gw
        .post_json(url, body(endpoint, &other.p256dh, &other.auth()))
        .await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(first["id"], second["id"], "同一个 endpoint 是更新");
    let stored = TursoPushRepo::new(gw.state().db.clone())
        .list()
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].p256dh, other.p256dh, "密钥换成新的");

    let (_, key_before) = gw.get_json("/v1/push/key").await;
    let public = key_before["public_key"].as_str().unwrap().to_string();
    assert_eq!(B64.decode(&public).unwrap().len(), 65);
    assert!(home.path().join("runtime/vapid.json").exists());

    for _ in 0..2 {
        let (status, _) = gw
            .request(
                reqwest::Method::DELETE,
                url,
                Some(serde_json::json!({ "endpoint": endpoint })),
            )
            .await;
        assert_eq!(status, 204, "退订幂等");
    }
    assert!(subscriptions(&gw).await.is_empty());

    gw.stop().await;
    let gw = home.start(FakeLlm::finisher("好")).await;
    let (_, key_after) = gw.get_json("/v1/push/key").await;
    assert_eq!(key_after["public_key"], public, "重启不换密钥");
    gw.stop().await;
}
