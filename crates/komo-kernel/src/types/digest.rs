//! 内容哈希。
//!
//! `plan_hash`（§7.2）、引用的内容哈希（§8.3）和配置指纹（§3）都要一个真哈希——绑定
//! 一份审批、判断一份输出有没有被改过，用的是同一个原语。实现来自 `sha2`：手写的
//! 散列哪怕对，也不该住在与密钥相邻的代码里。

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 一段内容的 SHA-256，十六进制小写。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    /// 对一段字节求哈希。
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(hex(&sha256(bytes)))
    }

    /// 对一段文本求哈希。
    pub fn of_str(text: &str) -> Self {
        Self::of_bytes(text.as_bytes())
    }

    /// 对一个 JSON 值的**规范化**形式求哈希：先转成 [`serde_json::Value`]，对象的键
    /// 因此落进 `BTreeMap` 按字典序排列，字段在结构体里的声明顺序不影响结果。
    pub fn of_json<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        let canonical = serde_json::to_value(value)?;
        Ok(Self::of_str(&serde_json::to_string(&canonical)?))
    }

    /// 不校验地包装一个已有的十六进制串（从数据库读回时用）。
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一段字节的 SHA-256 摘要。
pub fn sha256(message: &[u8]) -> [u8; 32] {
    Sha256::digest(message).into()
}

fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_test_vectors() {
        assert_eq!(
            ContentHash::of_str("").as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            ContentHash::of_str("abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            ContentHash::of_str("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
                .as_str(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hashes_a_message_longer_than_one_block() {
        let long = "a".repeat(1_000_000);
        assert_eq!(
            ContentHash::of_str(&long).as_str(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn json_hash_ignores_field_order() {
        let a: serde_json::Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"a":2,"b":1}"#).unwrap();
        assert_eq!(
            ContentHash::of_json(&a).unwrap(),
            ContentHash::of_json(&b).unwrap()
        );
    }
}
