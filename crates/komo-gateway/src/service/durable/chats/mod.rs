//! 对话（`docs/redesign.md` §0 P1 / P13 / P14 / P18）：新主页对话列的服务端。
//!
//! 一段"对话"就是一个会话：**主对话**是默认 Agent 的 `kind = main` 会话，**支线对话**是用户自己开的
//! `kind = normal` 会话（`POST /v1/sessions`，`source = web`）。存量的渠道 / 群会话
//! （`source` 以 `chat:` 开头）不是对话：不进列表，停住的运行也不往里投。
//!
//! - [`list`]：`GET /v1/chats` 的行（标题、最后一句话、时间、在等你、在跑）与排序；
//! - [`say`]：往对话里投一句话的原语（不起 Run，进下一轮上下文，同一个关联键只投一次）；
//! - [`attention`]：停住的运行（介入）在它所在的对话里开口，以及 `komo` 工具的 `intervention.*`。
//!
//! 这一层只读已有的会话、介入与 entry，不新增存储；唯一新写的是 `say` 落下的那条 entry 与会话行的
//! `last_message_at`。

pub mod attention;
pub mod list;
pub mod say;
#[cfg(test)]
mod tests;

use komo_kernel::durable::{SessionKind, SessionLifecycle, SessionRecord};

use super::DurableCore;
use super::sessions::SessionError;

/// 来源是渠道 / 群（`chat:{peer}`）的存量会话：不是对话。
pub fn is_channel_session(record: &SessionRecord) -> bool {
    record
        .source
        .as_deref()
        .is_some_and(|source| source.starts_with("chat:"))
}

/// 用户开的支线对话的来源（`POST /v1/sessions`）。
pub const WEB_SOURCE: &str = "web";

/// 一个会话上出现的话该落在哪段对话。
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// 这个会话本身（或沿 `parent_session` 往上找到的那个）就是一段活着的对话。
    Chat(Box<SessionRecord>),
    /// 默认 Agent 的主对话。
    Main,
}

/// 定时触发的会话（`kind = cron`）的结果与提问投到哪。
///
/// **3a 的接缝**：目标绑定了支线时改投那段支线（`docs/redesign.md` P22、§2.2）；现在没有绑定，一律主对话。
pub fn cron_route(_cron_session: &SessionRecord) -> Route {
    Route::Main
}

/// 一个任务会话往上最多找几层父会话（防环；正常只有一两层）。
const MAX_HOPS: usize = 8;

impl DurableCore {
    /// `record` 上的话落在哪段对话（P14）：
    ///
    /// - 主对话、支线对话：自己；
    /// - 任务会话：沿 `parent_session` 往上找到第一个非任务会话，再按它的种类定；
    /// - 定时触发的会话：[`cron_route`]；
    /// - 渠道 / 群的存量会话、已经不收输入的会话：主对话（它们不是列表里的对话）。
    pub async fn route_of(&self, record: &SessionRecord) -> Result<Route, SessionError> {
        let mut current = record.clone();
        for _ in 0..MAX_HOPS {
            let alive = current.lifecycle == SessionLifecycle::Active;
            match current.kind {
                SessionKind::Main | SessionKind::Normal => {
                    return Ok(if alive && !is_channel_session(&current) {
                        Route::Chat(Box::new(current))
                    } else {
                        Route::Main
                    });
                }
                SessionKind::Cron => return Ok(cron_route(&current)),
                SessionKind::Task => {
                    let Some(parent) = current.parent_session.as_deref() else {
                        return Ok(Route::Main);
                    };
                    match self.storage.session(parent).await? {
                        Some(parent) => current = parent,
                        None => return Ok(Route::Main),
                    }
                }
            }
        }
        Ok(Route::Main)
    }

    /// 默认 Agent 的名字（配置里的 `default_agent`）；Gateway 还没接上（只有测试里的裸核心）是 `None`。
    pub(super) fn default_agent(&self) -> Option<String> {
        self.gateway()
            .map(|state| state.snapshot().agent.default_agent.clone())
    }

    /// 把 [`Route`] 解成要落话的那段对话的会话行（主对话没有就建，同 `/v1/home-session`）。
    pub async fn resolve_route(&self, route: Route) -> Result<SessionRecord, SessionError> {
        match route {
            Route::Chat(record) => Ok(*record),
            Route::Main => {
                let agent = self.default_agent().ok_or_else(|| {
                    SessionError::Internal("Gateway 还没就绪，找不到主对话".into())
                })?;
                Ok(self.main_session(&agent).await?.0)
            }
        }
    }
}
