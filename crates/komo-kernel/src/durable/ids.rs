//! 数字 ID（docs/storage-durable.md §3.4）：全局单调、同一命名空间，ID 次序就是创建次序。
//!
//! 实体 ID 彼此是不同的类型，`Seq`（提交序号）又是另一个类型，混用编译不过。
//! 序列化成裸数字（`serde(transparent)`），线格式里没有品牌。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 能由 `Storage::mint_id` 铸出来的实体 ID。`Seq` 故意不实现它。
pub trait EntityId: Copy + From<u64> {
    fn raw(self) -> u64;
}

macro_rules! numeric_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

macro_rules! entity_id {
    ($(#[$meta:meta])* $name:ident) => {
        numeric_id!($(#[$meta])* $name);

        impl EntityId for $name {
            fn raw(self) -> u64 {
                self.0
            }
        }
    };
}

entity_id!(
    /// 一段 transcript 作用域；fork 出来的子会话是另一个 conversation。
    ConversationId
);
entity_id!(
    /// 不可变的 transcript 记录。
    EntryId
);
entity_id!(
    /// 持久任务状态机。
    TaskId
);
entity_id!(
    /// 收件箱里的一次提交。
    SubmissionId
);
entity_id!(
    /// 可变 JSON 状态的一次生命周期（每次重建换新 ID）。
    DocumentId
);
numeric_id!(
    /// 提交序号：严格递增，允许有空洞；不是实体 ID，不能由 `mint_id` 铸出。
    Seq
);

/// 根 conversation 固定是 1，`mint_id` 从 2 起。
pub const ROOT_CONVERSATION_ID: ConversationId = ConversationId::new(1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialize_as_bare_numbers() {
        assert_eq!(serde_json::to_string(&EntryId::new(7)).unwrap(), "7");
        assert_eq!(serde_json::from_str::<Seq>("9").unwrap(), Seq::new(9));
        assert_eq!(ROOT_CONVERSATION_ID.get(), 1);
    }
}
