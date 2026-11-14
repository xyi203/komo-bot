//! PKCE（RFC 7636）与登录里用的随机令牌：Gateway 的 OIDC 登录、设备登录和 CLI 的 `komo login`
//! 共用这一份，免得三处各写一遍 base64url 与 S256。

use crate::types::digest::sha256;

/// 32 字节随机数的 base64url（无填充，43 个字符）。来自操作系统随机源（两个 UUIDv4 拼起来）。
pub fn random_token() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64url(&bytes)
}

/// `code_challenge = BASE64URL(SHA256(code_verifier))`（方法 `S256`）。
pub fn s256_challenge(verifier: &str) -> String {
    base64url(&sha256(verifier.as_bytes()))
}

/// base64url，无填充。
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 63] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 附录 B 的向量。
    #[test]
    fn the_rfc_7636_challenge_vector_matches() {
        assert_eq!(
            s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_token_is_forty_three_url_safe_characters() {
        let token = random_token();
        assert_eq!(token.len(), 43);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(token, random_token());
    }
}
