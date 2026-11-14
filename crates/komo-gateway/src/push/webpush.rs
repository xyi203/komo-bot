//! Web Push 的两块密码学，直接用 `ring` 写（不引 web-push crate）：
//!
//! - **RFC 8291**（`aes128gcm` 内容加密）：[`encrypt`]。临时 ECDH P-256 密钥 + 浏览器的 `p256dh` / `auth`
//!   → HKDF-SHA256 → AES-128-GCM，单条记录，不加填充。
//! - **RFC 8292**（VAPID）：[`Vapid`]。ES256 JWT（`aud` / `exp` / `sub`），`Authorization: vapid t=…, k=…`。
//!
//! 全是纯函数 / 值类型，不碰网络与文件；发送在 [`super`]。

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
use ring::hkdf::{HKDF_SHA256, KeyType, Salt};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};

/// 单条记录的长度（RFC 8188 的 `rs`）。负载加上 17 字节开销必须放得进去；推送服务保证至少 4096。
const RECORD_SIZE: u32 = 4096;
/// 明文上限：记录长度 − 16（GCM 标签）− 1（结束分隔符）。
pub const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - 16 - 1;

#[derive(Debug, thiserror::Error)]
pub enum WebPushError {
    #[error("订阅里的 {0} 不合法")]
    BadSubscription(&'static str),
    #[error("负载太长（{0} 字节，上限 {MAX_PLAINTEXT}）")]
    TooLong(usize),
    #[error("加密失败")]
    Crypto,
    #[error("VAPID 密钥不可用：{0}")]
    Key(String),
}

struct Len(usize);

impl KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &ring::hkdf::Prk, info: &[u8], out: &mut [u8]) -> Result<(), WebPushError> {
    prk.expand(&[info], Len(out.len()))
        .and_then(|okm| okm.fill(out))
        .map_err(|_| WebPushError::Crypto)
}

fn decode(value: &str, what: &'static str) -> Result<Vec<u8>, WebPushError> {
    URL_SAFE_NO_PAD
        .decode(value.trim().trim_end_matches('='))
        .map_err(|_| WebPushError::BadSubscription(what))
}

/// 加密 `plaintext`，给出整段请求体（RFC 8188 头 + 密文）。`p256dh` / `auth` 是浏览器给的 base64url。
pub fn encrypt(plaintext: &[u8], p256dh: &str, auth: &str) -> Result<Vec<u8>, WebPushError> {
    let rng = SystemRandom::new();
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).map_err(|_| WebPushError::Crypto)?;
    let ephemeral = EphemeralPrivateKey::generate(&ECDH_P256, &rng).map_err(|_| WebPushError::Crypto)?;
    encrypt_with(plaintext, p256dh, auth, ephemeral, &salt)
}

/// [`encrypt`] 的核心，临时密钥与 salt 由调用方给（测试用 RFC 8291 附录 A 的固定值核对）。
fn encrypt_with(
    plaintext: &[u8],
    p256dh: &str,
    auth: &str,
    ephemeral: EphemeralPrivateKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, WebPushError> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(WebPushError::TooLong(plaintext.len()));
    }
    let ua_public = decode(p256dh, "p256dh")?;
    // 未压缩点：0x04 || X || Y。
    if ua_public.len() != 65 || ua_public[0] != 0x04 {
        return Err(WebPushError::BadSubscription("p256dh"));
    }
    let auth_secret = decode(auth, "auth")?;
    if auth_secret.len() != 16 {
        return Err(WebPushError::BadSubscription("auth"));
    }
    let as_public = ephemeral
        .compute_public_key()
        .map_err(|_| WebPushError::Crypto)?
        .as_ref()
        .to_vec();
    let shared = agree_ephemeral(
        ephemeral,
        &UnparsedPublicKey::new(&ECDH_P256, &ua_public),
        |secret| secret.to_vec(),
    )
    .map_err(|_| WebPushError::BadSubscription("p256dh"))?;

    // IKM = HKDF(salt = auth, ikm = ECDH, info = "WebPush: info\0" || ua || as, 32)
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&ua_public);
    key_info.extend_from_slice(&as_public);
    let prk_key = Salt::new(HKDF_SHA256, &auth_secret).extract(&shared);
    let mut ikm = [0u8; 32];
    expand(&prk_key, &key_info, &mut ikm)?;

    // CEK / NONCE = HKDF(salt = 随机 salt, ikm = IKM, info = …)
    let prk = Salt::new(HKDF_SHA256, salt).extract(&ikm);
    let mut cek = [0u8; 16];
    expand(&prk, b"Content-Encoding: aes128gcm\0", &mut cek)?;
    let mut nonce = [0u8; 12];
    expand(&prk, b"Content-Encoding: nonce\0", &mut nonce)?;

    // 明文后接 0x02：最后一条记录的分隔符（RFC 8188 §2），不另加填充。
    let mut record = Vec::with_capacity(plaintext.len() + 1 + 16);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).map_err(|_| WebPushError::Crypto)?);
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::empty(),
        &mut record,
    )
    .map_err(|_| WebPushError::Crypto)?;

    // 头：salt(16) || rs(4, BE) || idlen(1) || keyid（= 我们的临时公钥，65 字节）。
    let mut body = Vec::with_capacity(16 + 4 + 1 + as_public.len() + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

/// 服务端的 VAPID 密钥对（P-256）。
pub struct Vapid {
    pair: EcdsaKeyPair,
    /// 未压缩公钥的 base64url（网页订阅时的 `applicationServerKey`，也是 `k=`）。
    public_key: String,
}

impl std::fmt::Debug for Vapid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vapid").field("public_key", &self.public_key).finish_non_exhaustive()
    }
}

impl Vapid {
    /// 新生成一对，返回密钥与要落盘的 PKCS#8（DER）。
    pub fn generate() -> Result<(Vapid, Vec<u8>), WebPushError> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| WebPushError::Key("生成失败".into()))?;
        let bytes = pkcs8.as_ref().to_vec();
        Ok((Vapid::from_pkcs8(&bytes)?, bytes))
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Vapid, WebPushError> {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &rng)
            .map_err(|error| WebPushError::Key(error.to_string()))?;
        let public_key = URL_SAFE_NO_PAD.encode(pair.public_key().as_ref());
        Ok(Vapid { pair, public_key })
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// `Authorization` 头的值。`audience` 是推送服务的源（`https://fcm.googleapis.com`），`expires` 是
    /// unix 秒（不能晚于签发后 24 小时）。
    pub fn authorization(
        &self,
        audience: &str,
        subject: &str,
        expires: i64,
    ) -> Result<String, WebPushError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({ "aud": audience, "exp": expires, "sub": subject });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let signature = self
            .pair
            .sign(&SystemRandom::new(), signing_input.as_bytes())
            .map_err(|_| WebPushError::Crypto)?;
        // ES256 的 JWT 签名是 r || s（各 32 字节）：`FIXED` 变体正是这个形状。
        let jwt = format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()));
        Ok(format!("vapid t={jwt}, k={}", self.public_key))
    }
}
