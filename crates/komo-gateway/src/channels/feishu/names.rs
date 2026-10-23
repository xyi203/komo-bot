//! 话题上下文里发送者的名字（§11.3）。
//!
//! 话题列表接口的 `sender` 只有 `id` / `id_type`，没有名字；话题里被 @ 过的人从
//! `mentions` 就能查到，剩下的才走开放平台：用户 `contact/v3/users`，机器人
//! `application/v6/applications`（[`FeishuApi::user_name`] / [`FeishuApi::app_name`]）。
//!
//! 进程内缓存，同一个 id 只查一次：查到的记 [`NAME_TTL`]，被拒（没权限、不在通讯录可见
//! 范围、应用不存在）或名字为空的记 [`MISS_TTL`]——不然没权限的群每被 @ 一次就为同一批
//! 人白问一遍。**传输失败与超时不记**，下次再试。条目满 [`NAME_CACHE_CAP`] 时先清过期
//! 的，再清最早到期的。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::join_all;
use tokio::time::Instant;

use super::api::{FeishuApi, FeishuError, ThreadSender};

/// 查到的名字记多久。改名不常见，一天内显示旧名可以接受。
pub const NAME_TTL: Duration = Duration::from_secs(24 * 3600);

/// 查不到记多久。管理员补了权限，十分钟内生效。
pub const MISS_TTL: Duration = Duration::from_secs(600);

/// 最多记多少个 id。
pub const NAME_CACHE_CAP: usize = 1024;

/// 发送者是谁：决定走哪个接口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    App,
}

impl Kind {
    /// 按 `id_type` 认，缺了按前缀认；都认不出（`union_id` 之类）就不查。
    fn of(sender: &ThreadSender) -> Option<Self> {
        match sender.id_type.as_str() {
            "open_id" => Some(Kind::User),
            "app_id" => Some(Kind::App),
            "" if sender.id.starts_with("ou_") => Some(Kind::User),
            "" if sender.id.starts_with("cli_") => Some(Kind::App),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    /// `None` = 查过，查不到。
    name: Option<String>,
    until: Instant,
}

/// 发送者名字的进程内缓存。一个飞书渠道一份，与 [`FeishuApi`] 同寿命。
#[derive(Debug)]
pub struct SenderNames {
    entries: Mutex<HashMap<String, Entry>>,
    cap: usize,
}

impl Default for SenderNames {
    fn default() -> Self {
        Self::with_cap(NAME_CACHE_CAP)
    }
}

impl SenderNames {
    fn with_cap(cap: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            cap,
        }
    }

    /// `senders` 里每个能认出类型的 id 的名字。缓存里没有的并发去查，**最晚到 `deadline`**：
    /// 到点还没回来的就没有名字（调用方显示 id），已经回来的照样用上。
    pub async fn resolve<'a>(
        &self,
        api: &FeishuApi,
        senders: impl IntoIterator<Item = &'a ThreadSender>,
        deadline: Instant,
    ) -> HashMap<String, String> {
        let mut wanted: Vec<(&str, Kind)> = Vec::new();
        let mut seen = HashSet::new();
        for sender in senders {
            if let Some(kind) = Kind::of(sender)
                && !sender.id.is_empty()
                && seen.insert(sender.id.as_str())
            {
                wanted.push((sender.id.as_str(), kind));
            }
        }

        let missing: Vec<(&str, Kind)> = {
            let entries = self.entries.lock().expect("名字缓存");
            let now = Instant::now();
            wanted
                .iter()
                .copied()
                .filter(|(id, _)| !entries.get(*id).is_some_and(|entry| entry.until > now))
                .collect()
        };
        if !missing.is_empty() {
            let lookups = missing
                .iter()
                .map(|&(id, kind)| self.look_up(api, id, kind));
            if tokio::time::timeout_at(deadline, join_all(lookups))
                .await
                .is_err()
            {
                tracing::warn!("飞书：查发送者名字超时，没查到的显示 id");
            }
        }

        let entries = self.entries.lock().expect("名字缓存");
        wanted
            .iter()
            .filter_map(|(id, _)| {
                let name = entries.get(*id)?.name.clone()?;
                Some((id.to_string(), name))
            })
            .collect()
    }

    /// 查一个，结果直接进缓存——这样整批超时时，先回来的那些不白查。
    async fn look_up(&self, api: &FeishuApi, id: &str, kind: Kind) {
        let result = match kind {
            Kind::User => api.user_name(id).await,
            Kind::App => api.app_name(id).await,
        };
        let name = match result {
            Ok(name) => Some(name),
            Err(error @ (FeishuError::Api { .. } | FeishuError::Decode(_))) => {
                tracing::warn!(%error, id, "飞书：查不到发送者名字，显示 id");
                None
            }
            Err(error @ FeishuError::Transport(_)) => {
                tracing::warn!(%error, id, "飞书：查发送者名字失败，这次显示 id");
                return;
            }
        };
        self.remember(id, name);
    }

    fn remember(&self, id: &str, name: Option<String>) {
        let now = Instant::now();
        let until = now + if name.is_some() { NAME_TTL } else { MISS_TTL };
        let mut entries = self.entries.lock().expect("名字缓存");
        if entries.len() >= self.cap && !entries.contains_key(id) {
            entries.retain(|_, entry| entry.until > now);
            if entries.len() >= self.cap
                && let Some(soonest) = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.until)
                    .map(|(key, _)| key.clone())
            {
                entries.remove(&soonest);
            }
        }
        entries.insert(id.to_string(), Entry { name, until });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::fake::{Behavior, FakeOpenApi};

    fn sender(id: &str, id_type: &str) -> ThreadSender {
        ThreadSender {
            id: id.into(),
            id_type: id_type.into(),
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[tokio::test]
    async fn users_and_bots_are_named_by_their_own_endpoint() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        fake.host_user("ou_a", "张三");
        fake.host_app("cli_jeeson", "Jeeson Bot");
        let names = SenderNames::default();
        let senders = [sender("ou_a", "open_id"), sender("cli_jeeson", "app_id")];

        let got = names.resolve(&fake.api(), &senders, soon()).await;
        assert_eq!(got["ou_a"], "张三");
        assert_eq!(got["cli_jeeson"], "Jeeson Bot");
        assert_eq!(fake.calls_to("user_info")[0]["id"], "ou_a");
        assert_eq!(fake.calls_to("user_info")[0]["user_id_type"], "open_id");
        assert_eq!(fake.calls_to("app_info")[0]["id"], "cli_jeeson");
        assert_eq!(fake.calls_to("app_info")[0]["lang"], "zh_cn");
    }

    #[tokio::test]
    async fn a_known_name_and_a_known_miss_are_not_asked_again() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        fake.host_user("ou_a", "张三");
        let names = SenderNames::default();
        let api = fake.api();
        // 同一批里重复的 id 也只问一次。
        let senders = [
            sender("ou_a", "open_id"),
            sender("ou_a", "open_id"),
            sender("ou_gone", "open_id"),
        ];

        let first = names.resolve(&api, &senders, soon()).await;
        let second = names.resolve(&api, &senders, soon()).await;
        assert_eq!(first, second);
        assert_eq!(first.get("ou_a").map(String::as_str), Some("张三"));
        assert!(!first.contains_key("ou_gone"), "查不到就没有名字");
        assert_eq!(fake.calls_to("user_info").len(), 2, "每个 id 只问一次");
    }

    #[tokio::test]
    async fn a_refusal_leaves_the_id_unnamed_and_is_remembered() {
        let fake = FakeOpenApi::start(Behavior::refuse_names()).await;
        fake.host_app("cli_jeeson", "Jeeson Bot");
        let names = SenderNames::default();
        let api = fake.api();
        let senders = [sender("cli_jeeson", "app_id")];

        assert!(names.resolve(&api, &senders, soon()).await.is_empty());
        assert!(names.resolve(&api, &senders, soon()).await.is_empty());
        assert_eq!(fake.calls_to("app_info").len(), 1, "被拒也记下，不重问");
    }

    #[tokio::test]
    async fn a_slow_lookup_gives_up_at_the_deadline_and_is_retried_later() {
        let fake = FakeOpenApi::start(Behavior::slow_names(Duration::from_secs(2))).await;
        fake.host_user("ou_a", "张三");
        let names = SenderNames::default();
        let api = fake.api();
        // 先把 token 取好，超时只落在查名字上。
        api.tenant_token().await.unwrap();
        let senders = [sender("ou_a", "open_id")];

        let started = Instant::now();
        let got = names
            .resolve(&api, &senders, Instant::now() + Duration::from_millis(200))
            .await;
        assert!(got.is_empty());
        assert!(started.elapsed() < Duration::from_secs(1), "按时返回");

        names.resolve(&api, &senders, soon()).await;
        assert_eq!(fake.calls_to("user_info").len(), 2, "超时不记，下次再问");
    }

    #[tokio::test]
    async fn unknown_id_types_are_not_looked_up() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let names = SenderNames::default();
        let senders = [sender("on_union", "union_id"), sender("", "open_id")];
        assert!(
            names
                .resolve(&fake.api(), &senders, soon())
                .await
                .is_empty()
        );
        assert!(fake.calls_to("user_info").is_empty());
        assert!(fake.calls_to("app_info").is_empty());
    }

    #[test]
    fn a_full_cache_drops_the_soonest_to_expire_first() {
        let names = SenderNames::with_cap(2);
        names.remember("ou_hit", Some("张三".into()));
        names.remember("ou_miss", None);
        names.remember("ou_new", Some("李四".into()));
        let entries = names.entries.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains_key("ou_hit") && entries.contains_key("ou_new"));
    }
}
