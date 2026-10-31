//! 渠道篇幅规范（`docs/komo_bot.md` §4「渠道篇幅」，roadmap-muse S5）：回复要发到聊天渠道的 Run，
//! 输入之后追加一条宿主附上的短提示。
//!
//! **不放进系统段**：操作者的私聊（飞书、微信、Telegram）都落进同一个主会话，TUI 也能接上它；
//! 按渠道换系统段，home 里每换一次渠道就打掉整段前缀缓存。所以照记忆召回块（§9.4）的做法，每个 Run
//! 追加一次，只对回复要发到渠道的那条 Run。正文只看平台，同一平台逐字相同。
//!
//! 起因：DeepSeek Harness 的 Super-agent 一文实测"IM 单次回复不超过 100 词"有效；komo 在飞书里
//! 的回复常常是 TUI 那种几屏长的 markdown。

/// 这条提示所在 user 消息的 `komoOrigin`。
pub const CHANNEL_ORIGIN: &str = "channel";

/// 回复要发到 `platform`（`feishu` / `telegram` / `wechat` 或自定义渠道 id）时追加的那一段。
pub fn channel_reply_block(platform: &str) -> String {
    format!(
        "（宿主附注，不是用户说的：这一轮的回复会发到{}，对方多半在手机上看。）先给结论，\
         一般不超过约 150 字；要问就只问能直接回答的那一个问题。细节、表格、长清单写进文件给路径，\
         或者派成后台任务，别整段贴；用户明确要全文时照给。",
        platform_label(platform)
    )
}

fn platform_label(platform: &str) -> &str {
    match platform {
        "feishu" => "飞书",
        "telegram" => "Telegram",
        "wechat" => "微信",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正文只看平台：同一平台逐字相同（前缀稳定），平台名换成用户认得的叫法。
    #[test]
    fn the_block_names_the_platform_and_is_stable_per_platform() {
        let feishu = channel_reply_block("feishu");
        assert_eq!(feishu, channel_reply_block("feishu"));
        assert!(feishu.contains("发到飞书"), "{feishu}");
        assert!(feishu.contains("150 字"), "{feishu}");
        assert!(channel_reply_block("wechat").contains("发到微信"));
        assert!(channel_reply_block("slack").contains("发到slack"));
    }
}
