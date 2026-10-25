//! token 粗估。

/// 粗估 token 数：UTF-8 字节数 / 4，向上取整。
///
/// 对中日韩文字会**低估**（一个汉字 3 字节，实际常常是 1–2 个 token）。压缩的账看的是
/// 比例——摘要比原文短多少、上下文每次涨多少——同一把尺子量两边，偏差大体抵消。
pub fn estimate_tokens(text: &str) -> u64 {
    text.len().div_ceil(4) as u64
}

/// 一张图片按多少 token 记账（照 pi：4800 字符 ≈ 1200 token）。真实用量随分辨率变，压缩的账只要
/// 不把图片当成零。
pub const IMAGE_TOKENS: u64 = 1200;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_bytes_over_four_rounded_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("汉字"), 2, "6 字节");
    }
}
