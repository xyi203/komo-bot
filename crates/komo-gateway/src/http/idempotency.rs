//! 幂等请求键（§13.1「提交输入、审批、Cron 与 Memory 变更都支持幂等请求键；同一键
//! 对应不同内容则拒绝」）。
//!
//! **提交输入那一路不在这里**：它的幂等是持久的，由 `Ledger::accept_input` 按
//! `runs.request_key` 答（§8.5），重启也认。这里管的是另外三路——审批、Cron、Memory
//! 变更——它们在 state.db 里没有一张"请求键 → 结果"的表。
//!
// TODO(decide: 文档要求这三路也支持幂等键，但没有给它们一张表；`approval_requests`、
// `cron_jobs`、`memory_items` 都是按自己的主键幂等的，而不是按请求键。这里先用一个
// **进程内**的有界表：同键同内容返回原结果，同键不同内容 409。它挡得住客户端重发与
// 网络重试，挡不住跨重启的重发——那一种退化成"再执行一次"，而这三个动作本身都是幂等
// 的（已决定的审批返回原决定、Cron 按 id 覆盖、memory 的 confirm/forget 按 revision），
// 所以不会产生第二次副作用。要做成持久的需要一张表 → 见报告"需要编排者做"。)

use std::collections::VecDeque;
use std::sync::Mutex;

use komo_kernel::protocol::http::ErrorCode;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::RequestKey;

use super::error::ApiFailure;

/// 记住多少条。
const LIMIT: usize = 1_024;

/// 一条记下来的请求。
#[derive(Debug, Clone)]
struct Entry {
    key: RequestKey,
    body: ContentHash,
    response: serde_json::Value,
}

/// 请求键 → 上一次的结果。
#[derive(Debug, Default)]
pub struct Idempotency {
    entries: Mutex<VecDeque<Entry>>,
}

impl Idempotency {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这个键上一次的结果。**同键不同内容 → 409**。
    pub fn lookup<T: serde::de::DeserializeOwned>(
        &self,
        key: Option<&RequestKey>,
        body: &ContentHash,
    ) -> Result<Option<T>, ApiFailure> {
        let Some(key) = key else { return Ok(None) };
        let entries = self.entries.lock().expect("幂等表");
        let Some(entry) = entries.iter().rev().find(|entry| &entry.key == key) else {
            return Ok(None);
        };
        if &entry.body != body {
            return Err(ApiFailure::new(
                ErrorCode::RequestKeyConflict,
                format!("请求键 {key} 上一次对应的是另一份内容"),
            ));
        }
        match serde_json::from_value(entry.response.clone()) {
            Ok(value) => Ok(Some(value)),
            // 记下来的是另一个接口的响应：把它当作"同键不同内容"，而不是猜。
            Err(_) => Err(ApiFailure::new(
                ErrorCode::RequestKeyConflict,
                format!("请求键 {key} 上一次用在另一个接口上"),
            )),
        }
    }

    /// 记下这次的结果。
    pub fn remember<T: serde::Serialize>(
        &self,
        key: Option<&RequestKey>,
        body: &ContentHash,
        response: &T,
    ) {
        let Some(key) = key else { return };
        let Ok(value) = serde_json::to_value(response) else {
            return;
        };
        let mut entries = self.entries.lock().expect("幂等表");
        if entries.len() >= LIMIT {
            entries.pop_front();
        }
        entries.push_back(Entry {
            key: key.clone(),
            body: body.clone(),
            response: value,
        });
    }
}

/// 请求体的内容哈希——"同一键对应不同内容"比的就是它。
pub fn body_hash<T: serde::Serialize>(body: &T) -> ContentHash {
    ContentHash::of_str(&serde_json::to_string(body).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Reply {
        ok: bool,
    }

    #[test]
    fn the_same_key_with_the_same_body_returns_the_first_result() {
        let table = Idempotency::new();
        let key = RequestKey::new("k-1");
        let hash = body_hash(&"payload");
        table.remember(Some(&key), &hash, &Reply { ok: true });

        let again: Option<Reply> = table.lookup(Some(&key), &hash).unwrap();
        assert_eq!(again, Some(Reply { ok: true }));
    }

    #[test]
    fn the_same_key_with_a_different_body_is_a_conflict() {
        let table = Idempotency::new();
        let key = RequestKey::new("k-1");
        table.remember(Some(&key), &body_hash(&"one"), &Reply { ok: true });

        let error = table
            .lookup::<Reply>(Some(&key), &body_hash(&"two"))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::RequestKeyConflict);
    }

    #[test]
    fn no_key_means_no_memory() {
        let table = Idempotency::new();
        table.remember(None, &body_hash(&"one"), &Reply { ok: true });
        assert_eq!(
            table.lookup::<Reply>(None, &body_hash(&"one")).unwrap(),
            None
        );
    }
}
