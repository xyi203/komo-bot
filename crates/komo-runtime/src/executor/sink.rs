//! 流式输出写入器在 executor 与工具之间的交接。
//!
//! `Tool::execute(plan, ctx)` 没有 sink 参数——它在 kernel 里，这一波不改；而
//! [`ToolOutputStore::publish`](komo_kernel::traits::ToolOutputStore::publish) 要的
//! 正是那个 writer 的所有权，所以发布这一步必须留在 executor（§8.5 的顺序：先发布
//! 输出，再追加 `tool.result`）。两边都要碰同一个 writer，于是它寄存在这里：
//! executor 在 `start_call` 之后 `install`，`shell` / `python` 按自己的
//! `ctx.attempt` 取，execute 返回后 executor `take` 回来去 publish。
//!
//! 一次尝试只有一个 writer，键就是 [`AttemptId`]，所以两个工具不可能写串。
//!
//! （更干净的做法是 `Tool::execute` 多一个 `&mut dyn OutputWriter` 参数或
//! `ToolContext::sink()`；那是对 kernel 的改动，见报告。）

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use komo_kernel::traits::OutputWriter;
use komo_kernel::types::ids::AttemptId;

/// 一次尝试的写入器句柄。工具拿到的是它。
pub type SinkHandle = Arc<tokio::sync::Mutex<Box<dyn OutputWriter>>>;

/// 按 attempt 键的寄存处。克隆共享同一份。
#[derive(Clone, Default)]
pub struct AttemptSinks {
    slots: Arc<Mutex<BTreeMap<AttemptId, SinkHandle>>>,
}

impl std::fmt::Debug for AttemptSinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.slots.lock().expect("sink 寄存处").len();
        f.debug_struct("AttemptSinks")
            .field("open", &count)
            .finish()
    }
}

impl AttemptSinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// executor：`start_call` 之后把 writer 放进来。
    pub fn install(&self, attempt: &AttemptId, writer: Box<dyn OutputWriter>) -> SinkHandle {
        let handle: SinkHandle = Arc::new(tokio::sync::Mutex::new(writer));
        self.slots
            .lock()
            .expect("sink 寄存处")
            .insert(attempt.clone(), handle.clone());
        handle
    }

    /// 工具：按自己的 `ctx.attempt` 取。没有 = 这次执行不流式写输出。
    pub fn handle(&self, attempt: &AttemptId) -> Option<SinkHandle> {
        self.slots
            .lock()
            .expect("sink 寄存处")
            .get(attempt)
            .cloned()
    }

    /// executor：execute 返回后收回来去 publish。
    ///
    /// 工具没有释放自己那份句柄时返回 `None`——那种情况下发布会丢掉它写过的东西，
    /// 所以宁可让调用方看见"收不回来"，也不新开一个空 writer 假装无事发生。
    pub fn take(&self, attempt: &AttemptId) -> Option<Box<dyn OutputWriter>> {
        let handle = self.slots.lock().expect("sink 寄存处").remove(attempt)?;
        Arc::try_unwrap(handle)
            .ok()
            .map(tokio::sync::Mutex::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::test_support::MemOutputWriter;
    use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
    use komo_kernel::types::refs::AttemptRef;

    fn attempt_ref(attempt: &AttemptId) -> AttemptRef {
        AttemptRef {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            call: ToolCallId::from_raw("c"),
            attempt: attempt.clone(),
        }
    }

    #[tokio::test]
    async fn a_tool_writes_through_the_handle_and_the_executor_gets_it_back() {
        let sinks = AttemptSinks::new();
        let attempt = AttemptId::from_raw("a-1");
        let handle = sinks.install(
            &attempt,
            Box::new(MemOutputWriter::new(attempt_ref(&attempt))),
        );

        // 工具侧。
        {
            let tool_handle = sinks.handle(&attempt).expect("寄存着");
            tool_handle
                .lock()
                .await
                .write_stdout(b"hello")
                .await
                .unwrap();
        }
        drop(handle);

        let writer = sinks.take(&attempt).expect("收得回来");
        assert_eq!(writer.bytes_written(), 5);
        assert!(sinks.handle(&attempt).is_none(), "取走后寄存处就空了");
    }

    #[tokio::test]
    async fn two_attempts_do_not_share_a_writer() {
        let sinks = AttemptSinks::new();
        let one = AttemptId::from_raw("a-1");
        let two = AttemptId::from_raw("a-2");
        let h1 = sinks.install(&one, Box::new(MemOutputWriter::new(attempt_ref(&one))));
        let h2 = sinks.install(&two, Box::new(MemOutputWriter::new(attempt_ref(&two))));
        sinks
            .handle(&one)
            .unwrap()
            .lock()
            .await
            .write_stdout(b"1")
            .await
            .unwrap();
        drop(h1);
        drop(h2);
        assert_eq!(sinks.take(&one).unwrap().bytes_written(), 1);
        assert_eq!(sinks.take(&two).unwrap().bytes_written(), 0);
    }
}
