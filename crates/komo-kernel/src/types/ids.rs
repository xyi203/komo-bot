//! 标识符：全部是 UUIDv7 的 newtype，serde 透明。
//!
//! kernel 不读时钟（§13.4），所以没有 `new()`——只有 `new_at(now)`，时间由调用方
//! 从 [`crate::traits::Clock`] 取。UUIDv7 的前 48 位就是这个时间戳，因此同一
//! Session 里生成的 ID 天然按时间有序，便于排查。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::{NoContext, Timestamp, Uuid};

/// 解析标识符失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("不是合法的 UUID：{0}")]
pub struct IdParseError(String);

/// 由一个时刻生成 UUIDv7。
pub fn uuid_v7_at(now: OffsetDateTime) -> Uuid {
    let unix = now.unix_timestamp();
    // 1970 之前的时刻在 v7 里没有表示；夹到 0 而不是 panic。
    let seconds = u64::try_from(unix).unwrap_or(0);
    let nanos = now.nanosecond();
    Uuid::new_v7(Timestamp::from_unix(NoContext, seconds, nanos))
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// 由一个时刻生成一个新 ID。
            pub fn new_at(now: OffsetDateTime) -> Self {
                Self(uuid_v7_at(now).to_string())
            }

            /// 由一个已有的 UUID 构造。
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid.to_string())
            }

            /// 解析一个字符串，非 UUID 则报错。
            pub fn parse(raw: &str) -> Result<Self, IdParseError> {
                Uuid::parse_str(raw)
                    .map(Self::from_uuid)
                    .map_err(|_| IdParseError(raw.to_string()))
            }

            /// 不校验地包装一个字符串。只给测试替身与从数据库读回的行用。
            pub fn from_raw(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Self::parse(raw)
            }
        }
    };
}

id_type!(
    /// 一段连续对话、工作目录与上下文的载体（§8.1）。
    SessionId
);
id_type!(
    /// 一次用户输入或一次触发引起的持久任务；重启前后保持同一 ID（§8.1）。
    RunId
);
id_type!(
    /// 一次有独立执行状态的逻辑调用。provider 的 call_id 另存（§8.1）。
    ToolCallId
);
id_type!(
    /// `ToolCall` 的一次实际执行尝试（`tool_attempts`）。
    AttemptId
);
id_type!(
    /// 一条审批请求。短 ID 另见 [`ShortId`]。
    ApprovalId
);
id_type!(
    /// 一条主动投递记录（§11.4），补发按它幂等。
    DeliveryId
);
id_type!(
    /// 一个定时任务定义（§10）。
    CronJobId
);
id_type!(
    /// 一条自动记忆（§9.2）；内容版本另见 `revision`。
    MemoryId
);
id_type!(
    /// 一次被审核的操作（§7.1 的 `operation_id`）。
    OperationId
);
id_type!(
    /// 一条已生效的范围授权（`policy_grants`）。
    GrantId
);
id_type!(
    /// 一个执行者实例；领取代次绑定它（§8.7）。
    ExecutorId
);

/// 一条 Intervention 的 id（§7.5）。
///
/// **它不是一个新实体的主键**：清单是派生视图（`runs` 与 `approval_requests` 的并集
/// 查询，§7.5 第 1 条），这个 id 就是"用哪个句柄去答复"——审批类用短 ID（§11.3），
/// 另外两类用 Run ID。它存在只是为了让 `runs.wait_ref` 有一个可查的值，而不是又造
/// 一张会与权威漂移的表。
///
/// 它不是 UUID，所以没有 `new_at`：值由已有的事实派生出来。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InterventionId(String);

impl InterventionId {
    /// 不校验地包装。给"从数据库读回的行"与测试用。
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// 一个 Run 上最多停着**一条**要人判断的 Intervention——执行器在第一条结果不明的
    /// 调用上就停下，不会带着两个悬空的调用等人。所以 Run ID 本身就是它的句柄。
    pub fn for_run(run: &RunId) -> Self {
        Self(run.as_str().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InterventionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for InterventionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for InterventionId {
    type Err = std::convert::Infallible;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_raw(raw))
    }
}

/// 待处理集合内唯一的 4 位 base32 短 ID，聊天里用它答复审批（§11.3）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ShortId(String);

impl ShortId {
    /// Crockford base32 的字母表，去掉了容易混淆的 I / L / O / U。
    const ALPHABET: &'static [u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    /// 由一个 20 位整数渲染出 4 位短 ID。调用方负责在待处理集合内保证唯一。
    pub fn from_index(index: u32) -> Self {
        let mut out = [0u8; 4];
        let mut value = index;
        for slot in out.iter_mut().rev() {
            *slot = Self::ALPHABET[(value % 32) as usize];
            value /= 32;
        }
        Self(String::from_utf8(out.to_vec()).expect("字母表是 ASCII"))
    }

    /// 规范化用户输入（大写、去空白）后包装。
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized: String = raw.trim().to_ascii_uppercase();
        if normalized.len() == 4 && normalized.bytes().all(|b| Self::ALPHABET.contains(&b)) {
            Some(Self(normalized))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ShortId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Session 内按追加顺序严格递增的事件序号（§8.3）。由 JSONL 写入器分配。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    pub const ZERO: Seq = Seq(0);

    pub fn next(self) -> Seq {
        Seq(self.0 + 1)
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 一次逻辑事件的 ID。重复提交同一 ID 必须幂等，内容不同则报错（§8.3）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(String);

impl EventId {
    pub fn new_at(now: OffsetDateTime) -> Self {
        Self(uuid_v7_at(now).to_string())
    }

    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 幂等请求键（§8.5、§11.1）：`feishu:{event_id}` / `telegram:{update_id}` /
/// `wechat:{from}:{msg_id}` / HTTP 调用方自带的键。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestKey(String);

impl RequestKey {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn id_generated_at_a_time_is_a_v7_uuid() {
        let id = SessionId::new_at(datetime!(2026-09-15 08:00:00 UTC));
        let parsed = Uuid::parse_str(id.as_str()).expect("是 UUID");
        assert_eq!(parsed.get_version_num(), 7);
    }

    #[test]
    fn ids_generated_later_sort_after_earlier_ones() {
        let early = RunId::new_at(datetime!(2026-09-15 08:00:00 UTC));
        let late = RunId::new_at(datetime!(2026-09-15 09:00:00 UTC));
        assert!(early.as_str() < late.as_str());
    }

    #[test]
    fn id_serializes_as_a_bare_string() {
        let id = SessionId::from_raw("sess-1");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"sess-1\"");
        let back: SessionId = serde_json::from_str("\"sess-1\"").unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn parse_rejects_a_non_uuid() {
        assert!(SessionId::parse("not-a-uuid").is_err());
    }

    #[test]
    fn short_id_is_four_base32_characters() {
        assert_eq!(ShortId::from_index(0).as_str(), "0000");
        assert_eq!(ShortId::from_index(1).as_str(), "0001");
        assert_eq!(ShortId::from_index(32).as_str(), "0010");
        assert_eq!(ShortId::parse(" 7k2m ").unwrap().as_str(), "7K2M");
        assert!(ShortId::parse("7K2").is_none());
        assert!(ShortId::parse("7K2I").is_none(), "I 不在字母表里");
    }
}
