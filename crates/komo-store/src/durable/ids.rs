//! ID 分配（§3.4）：conversation / entry / task / submission / document 共用一个全局
//! 单调数字命名空间（pi-durable spec §10 `mintId`）。
//!
//! 计数器在开库时取各实体表的最大 ID，之后只在内存里递增。两种取号方式：
//!
//! - [`crate::durable::Line::commit_with`]：闭包在**线内**拿到 [`IdMint`]，取号顺序就是
//!   提交顺序——ID 次序等于创建次序，正是今天 UUIDv7 同毫秒乱序要另加 `seq` 的那个坑；
//! - [`crate::durable::Line::mint_id`]：先取号再组写入，给"ID 要先写进别处（blob 路径）"的
//!   场景。并发调用方之间取号序与提交序可以不同，所以需要严格次序时用前一种。
//!
//! 取了号没提交只是留一个空洞，不影响单调。

use std::sync::atomic::{AtomicI64, Ordering};

/// 线内取号器。
#[derive(Debug)]
pub struct IdMint<'a> {
    counter: &'a AtomicI64,
}

impl<'a> IdMint<'a> {
    pub(crate) fn new(counter: &'a AtomicI64) -> Self {
        Self { counter }
    }

    /// 取下一个 ID（从 1 起）。
    pub fn mint(&mut self) -> i64 {
        next(self.counter)
    }
}

/// 计数器里存"已分配的最大 ID"，`fetch_add` 返回旧值，所以新 ID 是旧值 + 1。
pub(crate) fn next(counter: &AtomicI64) -> i64 {
    counter.fetch_add(1, Ordering::SeqCst) + 1
}

/// 同 [`next`]，但到 `i64::MAX` 就停（`None`），不回绕。
pub(crate) fn try_next(counter: &AtomicI64) -> Option<i64> {
    counter
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |c| c.checked_add(1))
        .ok()
        .map(|old| old + 1)
}
