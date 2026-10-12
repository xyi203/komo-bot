//! 微信纯文本渲染与入站去重键（§11.3）。
//!
//! 微信这一列与另外两个渠道的差别不在内容，在**能力**：iLink 只有纯文本，没有卡片、
//! 没有按钮、也没有"编辑已发出的那一条"。**超长截断，并提示到 TUI 看全文**（§11.3
//! 微信列的原话）。审批的纯文本请求与短 ID 命令随审批一起删掉了（§7，2026-10-11）。
//!
//! 这里还住着入站去重键的回退哈希（[`fallback_request_key`]）。它和渲染放在一起不是
//! 凑巧：两者都是"把一条消息变成一段确定的文本"的纯函数，都要能在没有网络、没有
//! SDK 的情况下逐字断言。

use komo_kernel::types::chat::Outbound;
use komo_kernel::types::digest::ContentHash;

/// 一条微信消息的正文上限，按**字符**计。
///
// TODO(decide: iLink 没有公开接口文档，服务端的真实上限未知。SDK 自己发长文本时按
// 4000 **字节**切块（`bot.rs::chunk_text`），所以 4000 字节是目前唯一有依据的数字；
// 这里取 1800 字符，对全中文正文约 5.4KB、对全 ASCII 约 1.8KB——比 4000 字节保守，
// 因为 §11.3 微信列要的是"截断并提示到 TUI"，宁可早截也不要发出去被服务端整条拒掉。)
pub const MESSAGE_LIMIT: usize = 1800;

/// 截断处留下的那句话。§11.3 微信列：**提示到 TUI 看全文**。
pub const TRUNCATION_NOTE: &str = "…（已截断，全文到 TUI 看）";

/// 回退去重键的时间窗，毫秒（§11.1）。
pub const FALLBACK_WINDOW_MS: i64 = 60_000;

/// 回退去重键里内容哈希取多少个十六进制位。
const FALLBACK_HASH_PREFIX: usize = 16;

/// 一个 [`Outbound`] 渲染成的那一段纯文本。
///
/// **只有一段**：微信这一列是截断，不是分段（§11.3）。
pub fn render(outbound: &Outbound) -> String {
    match outbound {
        Outbound::Text { text } => truncate(text, MESSAGE_LIMIT),
        Outbound::RunFinished { summary, .. } => truncate(summary, MESSAGE_LIMIT),
        Outbound::NeedsAttention { reason, .. } => {
            truncate(&format!("⚠️ 需要你判断\n\n{reason}"), MESSAGE_LIMIT)
        }
    }
}

/// 超过 `limit` 个字符就截断，并在末尾写上 [`TRUNCATION_NOTE`]。
pub fn truncate(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            out.push_str(TRUNCATION_NOTE);
            return out;
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------- 去重键

/// `client_id` 为空时的回退去重键：`wechat:{from}:{内容哈希}:{60s 窗口}`（§11.1）。
///
/// iLink 的 wire 消息没有 `msg_id`，逐消息的唯一标识只有 `client_id`——而那是**发送方**
/// 微信客户端生成的 UUID，komo 既不能假设它全局唯一，也不能假设它非空（类型是
/// `String` 而不是 `Option<String>`）。空的时候只剩下三样东西是确定的：谁发的、说了
/// 什么、大概什么时候。
///
/// 窗口取 60 秒是有代价的，而且这个代价必须写下来：**同一个人在同一分钟里把同一句话
/// 说两遍，会被当成一次重投吞掉一条。** 这是刻意的取舍——微信的重投有两个来源（游标
/// 不持久化、服务端回了消息却回空游标），两者都会在秒级内把同一批消息原样再交一次，
/// 而"同一分钟内重复说同一句话"在聊天里既少见又无害（再说一遍即可）。反过来放宽窗口
/// 则会让一次真实的重投溜进 Run，那是真的执行两次。
///
/// 窗口是**定长的桶**（`create_time_ms / 60000`），不是滑动窗口，所以贴着桶边界的两条
/// 消息会落进不同的键。这对重投**不成问题**：`create_time_ms` 是 wire 上的字段，是消息
/// **原本**的发送时刻，重投时原样带回来，落的是同一个桶；只有服务端在重投时重写这个
/// 字段才会漏掉一次——而那时它连"同一条消息"都不成立了。
///
/// 哈希用 kernel 的 [`ContentHash`]（SHA-256），取前 16 位十六进制：键要进数据库、要
/// 进日志，全长 64 位只是噪音，而 64 bit 的碰撞面对"同一个人、同一分钟"这个前提已经
/// 远远够用。
pub fn fallback_request_key(from: &str, text: &str, create_time_ms: i64) -> String {
    let digest = ContentHash::of_str(text);
    let hash = &digest.as_str()[..FALLBACK_HASH_PREFIX.min(digest.as_str().len())];
    // `div_euclid` 而不是 `/`：create_time_ms 理论上可以是负数（服务端时钟乱跳），
    // 而截断除法在零两侧不单调，会让相邻的两毫秒落进同一个窗口号。
    let window = create_time_ms.div_euclid(FALLBACK_WINDOW_MS);
    format!("wechat:{from}:{hash}:{window}")
}

/// 主去重键：`wechat:{from_user_id}:{client_id}`（§11.1）。
pub fn primary_request_key(from: &str, client_id: &str) -> String {
    format!("wechat:{from}:{client_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use komo_kernel::types::chat::ChannelPlatform;
    use komo_kernel::types::ids::{RunId, SessionId};

    // ⑤ 超长截断 + 「到 TUI 看全文」提示。
    #[test]
    fn an_oversized_body_is_truncated_with_a_pointer_to_the_tui() {
        let body = "长".repeat(MESSAGE_LIMIT * 2);
        let text = render(&Outbound::Text { text: body });
        assert!(text.ends_with(TRUNCATION_NOTE), "{text}");
        assert!(text.contains("TUI"), "提示必须说去哪看全文：{text}");
        assert_eq!(
            text.chars().count(),
            MESSAGE_LIMIT + TRUNCATION_NOTE.chars().count()
        );
    }

    #[test]
    fn a_short_body_is_left_alone() {
        let text = render(&Outbound::Text { text: "在".into() });
        assert_eq!(text, "在");
        assert!(!text.contains(TRUNCATION_NOTE));
    }

    #[test]
    fn a_run_summary_and_an_attention_notice_both_render() {
        let finished = Outbound::RunFinished {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            summary: "跑完了".into(),
        };
        assert_eq!(render(&finished), "跑完了");

        let attention = Outbound::NeedsAttention {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            reason: "磁盘满了".into(),
        };
        assert!(render(&attention).contains("磁盘满了"));
        assert!(render(&attention).contains("需要你判断"));
    }

    // ② 回退键：含 from + 内容哈希 + 60s 窗口；同一分钟内同文本同键、跨分钟不同键。
    #[test]
    fn the_fallback_key_is_sender_content_and_a_sixty_second_window() {
        // 一个整分的起点，好让"这一分钟之内"是一句确切的话。
        const MINUTE: i64 = FALLBACK_WINDOW_MS;
        const BASE: i64 = 29_333_333 * MINUTE;

        let a = fallback_request_key("wxid_op", "在吗", BASE);
        let b = fallback_request_key("wxid_op", "在吗", BASE + MINUTE - 1);
        assert_eq!(a, b, "同一分钟内同一句话是同一个键");

        let next_minute = fallback_request_key("wxid_op", "在吗", BASE + MINUTE);
        assert_ne!(a, next_minute, "跨分钟是另一个键");

        let other_text = fallback_request_key("wxid_op", "在么", BASE);
        assert_ne!(a, other_text, "换一句话是另一个键");

        let other_sender = fallback_request_key("wxid_x", "在吗", BASE);
        assert_ne!(a, other_sender, "换一个人是另一个键");

        assert!(a.starts_with("wechat:wxid_op:"), "{a}");
        let hash = ContentHash::of_str("在吗");
        assert!(a.contains(&hash.as_str()[..16]), "{a}");
        assert!(a.ends_with(&(BASE / MINUTE).to_string()), "{a}");
    }

    #[test]
    fn the_fallback_window_does_not_fold_across_zero() {
        // 服务端时钟乱跳到 1970 年之前时，截断除法会让 -1ms 与 +1ms 落进同一个窗口。
        assert_ne!(
            fallback_request_key("wxid_op", "在吗", -1),
            fallback_request_key("wxid_op", "在吗", 1)
        );
    }

    #[test]
    fn the_primary_key_is_sender_and_client_id() {
        assert_eq!(
            primary_request_key("wxid_op", "3f2b-uuid"),
            "wechat:wxid_op:3f2b-uuid"
        );
    }

    #[test]
    fn a_peer_is_a_wechat_user() {
        // 微信只有 DM：会话就是那个人（§11.2）。
        let peer = komo_kernel::types::chat::ChannelPeer::new(ChannelPlatform::Wechat, "wxid_op");
        assert_eq!(peer.to_string(), "wechat:wxid_op");
    }
}
