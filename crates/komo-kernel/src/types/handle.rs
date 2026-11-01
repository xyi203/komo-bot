//! 任务与监听的短号（`docs/storage-durable-d5.md` 决定 5，2026-10-31 改）：6 位小写字母加数字，
//! 例如 `k3x9qa`。
//!
//! 短号由全局 ID（任务 = 子 conversation ID，监听 = 挂它的工具任务 ID，两者同一条 ID 线、不会撞）
//! 经一个**固定密钥的可逆置换**再转 base36 得到：
//!
//! - 看起来是随机的，相邻两个任务的短号不挨着；
//! - 同一个 ID 永远得到同一个短号，重启不变，不加列、不迁移老数据；
//! - 反过来直接算回 ID（[`parse_handle`]），解析不用遍历调用方的子任务。
//!
//! 置换是 36⁶ 上的 4 轮 Feistel（两半各 36³，轮函数 splitmix64）。ID ≥ 36⁶（约 21.8 亿）时，
//! 商用普通 base36 写在前面，短号随之变长——不设上限。**算法与 [`KEY`] 一旦发布就不能改**：
//! 改了，模型手里、聊天记录里的短号就对不上了。测试里钉了几个值。
//!
//! 过渡：老的纯数字写法（`#980`）解析时还收，不再往外给。6 位全是数字的串两种读法都试，由调用方
//! 在自己的范围里认（[`parse_handle`] 给出候选）。

/// 36³：Feistel 每一半的取值范围。
const HALF: u64 = 46_656;
/// 36⁶：6 位短号的取值范围。
const DOMAIN: u64 = HALF * HALF;
/// 短号的位数（ID < 36⁶ 时）。
pub const HANDLE_LEN: usize = 6;
/// 置换的密钥。不能改，见模块文档。
const KEY: u64 = 0x6b6f_6d6f_5f68_646c;
const ROUNDS: u64 = 4;
const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// 一个 ID 的短号（不带 `#`）。
pub fn short_handle(id: u64) -> String {
    let (high, low) = (id / DOMAIN, id % DOMAIN);
    let mut out = if high > 0 {
        base36(high)
    } else {
        String::new()
    };
    let code = base36(permute(low));
    out.push_str(&"0".repeat(HANDLE_LEN - code.len()));
    out.push_str(&code);
    out
}

/// 用户 / 模型写来的短号可能指的 ID（去掉首尾空白与一个 `#`，不分大小写）：
///
/// - 至少 6 位、只有字母数字：按短号算回的那个 ID；
/// - 全是数字：老写法，数字本身也是一个候选。
///
/// 两种都成立（6 位以上全是数字）时两个都给，由调用方在自己的范围里认。认不出是空。
pub fn parse_handle(raw: &str) -> Vec<u64> {
    let text = raw
        .trim()
        .trim_start_matches('#')
        .trim()
        .to_ascii_lowercase();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Vec::new();
    }
    let mut out = Vec::new();
    if text.len() >= HANDLE_LEN {
        let (prefix, code) = text.split_at(text.len() - HANDLE_LEN);
        let high = if prefix.is_empty() {
            Some(0)
        } else if prefix.starts_with('0') {
            None
        } else {
            from_base36(prefix)
        };
        let id = high
            .zip(from_base36(code))
            .and_then(|(high, low)| high.checked_mul(DOMAIN)?.checked_add(unpermute(low)));
        out.extend(id);
    }
    if text.bytes().all(|b| b.is_ascii_digit())
        && let Ok(legacy) = text.parse::<u64>()
        && !out.contains(&legacy)
    {
        out.push(legacy);
    }
    out
}

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn round(i: u64, half: u64) -> u64 {
    mix(KEY ^ (i << 56) ^ half) % HALF
}

fn permute(x: u64) -> u64 {
    let (mut left, mut right) = (x / HALF, x % HALF);
    for i in 0..ROUNDS {
        let next = (left + round(i, right)) % HALF;
        left = right;
        right = next;
    }
    left * HALF + right
}

fn unpermute(y: u64) -> u64 {
    let (mut left, mut right) = (y / HALF, y % HALF);
    for i in (0..ROUNDS).rev() {
        let previous = (right + HALF - round(i, left)) % HALF;
        right = left;
        left = previous;
    }
    left * HALF + right
}

fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("ASCII")
}

fn from_base36(text: &str) -> Option<u64> {
    text.bytes().try_fold(0u64, |acc, b| {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'z' => b - b'a' + 10,
            _ => return None,
        };
        acc.checked_mul(36)?.checked_add(u64::from(digit))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 钉住的值：算法或密钥变了这里会红——那意味着已经发出去的短号全对不上了。
    #[test]
    fn handles_are_pinned() {
        let pinned: Vec<(u64, String)> = [1, 12, 980, 981]
            .into_iter()
            .map(|id| (id, short_handle(id)))
            .collect();
        for (id, handle) in &pinned {
            assert_eq!(handle.len(), HANDLE_LEN, "{id} → {handle}");
        }
        assert_eq!(
            pinned,
            vec![
                (1, PINNED[0].into()),
                (12, PINNED[1].into()),
                (980, PINNED[2].into()),
                (981, PINNED[3].into()),
            ]
        );
    }

    const PINNED: [&str; 4] = ["b1kg2a", "1lxxbx", "7ddfne", "n0vbpx"];

    /// 算得回来：6 位范围内、跨过 36⁶ 变长之后都一样；带 `#`、大写、空白也认。
    #[test]
    fn a_handle_parses_back_to_its_id() {
        for id in [
            1,
            2,
            980,
            46_655,
            46_656,
            DOMAIN - 1,
            DOMAIN,
            DOMAIN * 37 + 5,
        ] {
            let handle = short_handle(id);
            assert!(parse_handle(&handle).contains(&id), "{id} → {handle}");
            let loose = format!("  #{} ", handle.to_ascii_uppercase());
            assert!(parse_handle(&loose).contains(&id), "{loose}");
        }
        assert_eq!(short_handle(DOMAIN).len(), HANDLE_LEN + 1);
    }

    /// 相邻 ID 的短号不挨着，前 2000 个没有重复。
    #[test]
    fn neighbouring_ids_get_unrelated_handles() {
        let handles: std::collections::HashSet<String> = (1..=2000).map(short_handle).collect();
        assert_eq!(handles.len(), 2000);
        let (a, b) = (short_handle(980), short_handle(981));
        assert_ne!(a[..4], b[..4], "{a} {b}");
    }

    /// 老的纯数字写法还收；6 位以上全数字两种读法都给；认不出的是空。
    #[test]
    fn legacy_numbers_still_parse_and_junk_does_not() {
        assert_eq!(parse_handle("#980"), vec![980]);
        assert_eq!(parse_handle("12"), vec![12]);
        let both = parse_handle("123456");
        assert_eq!(both.len(), 2, "{both:?}");
        assert!(both.contains(&123_456));
        for junk in ["", "#", "k3x9-a", "汉字汉字汉字", "0abcdefg"] {
            assert!(parse_handle(junk).is_empty(), "{junk}");
        }
    }
}
