//! Channel / Inbound 实现：飞书、Telegram、WeChat（§11.1、§11.5、§13.5）。

#[cfg(feature = "feishu")]
pub mod feishu;
#[cfg(feature = "telegram")]
pub mod telegram;
#[cfg(feature = "wechat")]
pub mod wechat;
