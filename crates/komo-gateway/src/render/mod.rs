//! 审批消息渲染；渠道之间的差别只在渲染，不在决策（§11.3）。

#[cfg(feature = "feishu")]
pub mod feishu;
#[cfg(feature = "telegram")]
pub mod telegram;
#[cfg(feature = "wechat")]
pub mod wechat;
