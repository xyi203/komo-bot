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
    let ephemeral =
        EphemeralPrivateKey::generate(&ECDH_P256, &rng).map_err(|_| WebPushError::Crypto)?;
    encrypt_with(plaintext, p256dh, auth, ephemeral, &salt)
}

/// [`encrypt`] 的核心：生成密钥协商之后的纯计算部分在 [`seal`]。
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
    seal(
        plaintext,
        &ua_public,
        &auth_secret,
        &as_public,
        &shared,
        salt,
    )
}

/// ECDH 之后的部分：HKDF 派生 → AES-128-GCM → RFC 8188 头。纯函数，RFC 8291 附录 A 的固定向量直接喂它
/// （`ring` 不允许从字节造临时私钥，向量里的 ECDH 共享密钥只能在这一层注入）。
fn seal(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    as_public: &[u8],
    shared: &[u8],
    salt: &[u8; 16],
) -> Result<Vec<u8>, WebPushError> {
    // IKM = HKDF(salt = auth, ikm = ECDH, info = "WebPush: info\0" || ua || as, 32)
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let prk_key = Salt::new(HKDF_SHA256, auth_secret).extract(shared);
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
    let key =
        LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).map_err(|_| WebPushError::Crypto)?);
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
    body.extend_from_slice(as_public);
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
        f.debug_struct("Vapid")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
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
        let jwt = format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        );
        Ok(format!("vapid t={jwt}, k={}", self.public_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey as SigPublicKey};

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).expect("向量是合法 base64url")
    }

    // RFC 8291 附录 A 的固定向量。
    const PLAINTEXT: &[u8] = b"When I grow up, I want to be a watermelon";
    const UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH_SECRET: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const AS_PUBLIC: &str =
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const ECDH_SECRET: &str = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const BODY: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    fn salt() -> [u8; 16] {
        b64(SALT).try_into().unwrap()
    }

    #[test]
    fn the_rfc_8291_appendix_a_vector_is_reproduced_byte_for_byte() {
        let body = seal(
            PLAINTEXT,
            &b64(UA_PUBLIC),
            &b64(AUTH_SECRET),
            &b64(AS_PUBLIC),
            &b64(ECDH_SECRET),
            &salt(),
        )
        .unwrap();
        assert_eq!(URL_SAFE_NO_PAD.encode(&body), BODY);
        // 头：salt、rs = 4096、idlen = 65、keyid = as_public。
        assert_eq!(&body[..16], &b64(SALT)[..]);
        assert_eq!(&body[16..20], &[0, 0, 0x10, 0]);
        assert_eq!(body[20], 65);
        assert_eq!(&body[21..86], &b64(AS_PUBLIC)[..]);
        assert_eq!(body.len(), 86 + PLAINTEXT.len() + 1 + 16);
    }

    #[test]
    fn a_wrong_auth_secret_or_salt_changes_the_ciphertext() {
        let good = URL_SAFE_NO_PAD.encode(
            seal(
                PLAINTEXT,
                &b64(UA_PUBLIC),
                &b64(AUTH_SECRET),
                &b64(AS_PUBLIC),
                &b64(ECDH_SECRET),
                &salt(),
            )
            .unwrap(),
        );
        let other_auth = seal(
            PLAINTEXT,
            &b64(UA_PUBLIC),
            &[7u8; 16],
            &b64(AS_PUBLIC),
            &b64(ECDH_SECRET),
            &salt(),
        )
        .unwrap();
        assert_ne!(URL_SAFE_NO_PAD.encode(other_auth), good);
        let other_salt = seal(
            PLAINTEXT,
            &b64(UA_PUBLIC),
            &b64(AUTH_SECRET),
            &b64(AS_PUBLIC),
            &b64(ECDH_SECRET),
            &[1u8; 16],
        )
        .unwrap();
        assert_ne!(URL_SAFE_NO_PAD.encode(other_salt), good);
    }

    #[test]
    fn a_real_encryption_has_the_rfc_8188_shape_and_fresh_randomness() {
        let a = encrypt(PLAINTEXT, UA_PUBLIC, AUTH_SECRET).unwrap();
        let b = encrypt(PLAINTEXT, UA_PUBLIC, AUTH_SECRET).unwrap();
        assert_ne!(a, b, "salt 与临时密钥每次都换");
        assert_eq!(a.len(), 86 + PLAINTEXT.len() + 1 + 16);
        assert_eq!(&a[16..20], &4096u32.to_be_bytes());
        assert_eq!(a[20], 65);
        assert_eq!(a[21], 0x04, "keyid 是未压缩的临时公钥");
        // 带 `=` 填充的 base64url 也认（浏览器偶尔这样给）。
        assert!(encrypt(PLAINTEXT, UA_PUBLIC, &format!("{AUTH_SECRET}==")).is_ok());
    }

    #[test]
    fn the_longest_payload_fits_one_record_and_one_more_byte_is_refused() {
        let max = vec![b'x'; MAX_PLAINTEXT];
        let body = encrypt(&max, UA_PUBLIC, AUTH_SECRET).unwrap();
        // 头 86 字节 + 整条记录正好 rs = 4096 字节。
        assert_eq!(body.len(), 86 + 4096);
        let over = vec![b'x'; MAX_PLAINTEXT + 1];
        assert!(matches!(
            encrypt(&over, UA_PUBLIC, AUTH_SECRET),
            Err(WebPushError::TooLong(n)) if n == MAX_PLAINTEXT + 1
        ));
    }

    #[test]
    fn malformed_subscription_keys_are_refused() {
        let compressed = URL_SAFE_NO_PAD.encode([&[0x02u8][..], &[1u8; 32][..]].concat());
        let wrong_prefix = {
            let mut key = b64(UA_PUBLIC);
            key[0] = 0x05;
            URL_SAFE_NO_PAD.encode(key)
        };
        let off_curve = URL_SAFE_NO_PAD.encode([&[0x04u8][..], &[1u8; 64][..]].concat());
        for bad in [
            "",
            "!!!",
            compressed.as_str(),
            wrong_prefix.as_str(),
            off_curve.as_str(),
        ] {
            assert!(
                matches!(
                    encrypt(PLAINTEXT, bad, AUTH_SECRET),
                    Err(WebPushError::BadSubscription("p256dh"))
                ),
                "p256dh {bad:?}"
            );
        }
        let short = URL_SAFE_NO_PAD.encode([0u8; 15]);
        let long = URL_SAFE_NO_PAD.encode([0u8; 17]);
        for bad in ["", "***", short.as_str(), long.as_str()] {
            assert!(
                matches!(
                    encrypt(PLAINTEXT, UA_PUBLIC, bad),
                    Err(WebPushError::BadSubscription("auth"))
                ),
                "auth {bad:?}"
            );
        }
    }

    #[test]
    fn a_vapid_header_verifies_with_the_published_key_and_carries_the_claims() {
        let (vapid, pkcs8) = Vapid::generate().unwrap();
        let header = vapid
            .authorization(
                "https://push.example.com",
                "mailto:ops@example.com",
                1_900_000_000,
            )
            .unwrap();
        let rest = header
            .strip_prefix("vapid t=")
            .expect("格式 vapid t=…, k=…");
        let (jwt, key) = rest.split_once(", k=").expect("带 k=");
        assert_eq!(key, vapid.public_key());
        assert_eq!(b64(key).len(), 65);
        assert_eq!(b64(key)[0], 0x04);

        let mut parts = jwt.split('.');
        let (head, claims, signature) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );
        assert!(parts.next().is_none());
        let head: serde_json::Value = serde_json::from_slice(&b64(head)).unwrap();
        assert_eq!(head, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
        let claims_json: serde_json::Value = serde_json::from_slice(&b64(claims)).unwrap();
        assert_eq!(
            claims_json,
            serde_json::json!({
                "aud": "https://push.example.com",
                "exp": 1_900_000_000,
                "sub": "mailto:ops@example.com"
            })
        );
        let signature = b64(signature);
        assert_eq!(signature.len(), 64, "ES256 是 r||s 各 32 字节");
        let message = format!("{}.{claims}", jwt.split('.').next().unwrap());
        let verifier = SigPublicKey::new(&ECDSA_P256_SHA256_FIXED, b64(key));
        verifier
            .verify(message.as_bytes(), &signature)
            .expect("验签通过");
        // 改一个字节就验不过。
        let tampered = format!("{message}x");
        assert!(verifier.verify(tampered.as_bytes(), &signature).is_err());
        let forged = format!(
            "{}.{}",
            jwt.split('.').next().unwrap(),
            URL_SAFE_NO_PAD.encode(b"{\"aud\":\"https://evil\"}")
        );
        assert!(verifier.verify(forged.as_bytes(), &signature).is_err());

        // 落盘的 PKCS#8 读回来是同一把公钥。
        assert_eq!(
            Vapid::from_pkcs8(&pkcs8).unwrap().public_key(),
            vapid.public_key()
        );
        assert!(matches!(
            Vapid::from_pkcs8(b"garbage"),
            Err(WebPushError::Key(_))
        ));
    }
}
