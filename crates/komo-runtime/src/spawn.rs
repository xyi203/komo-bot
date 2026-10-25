//! 刚写完的可执行文件立刻执行：`ETXTBSY`（`Text file busy`）的有界重试。
//!
//! 多线程进程里，一个线程以写方式打开可执行文件、写完关闭之前，另一个线程 fork 出的子进程
//! 会继承那个写 fd（`O_CLOEXEC` 只在 exec 时才关，fork 到 exec 之间它还开着）。这时对这个
//! 文件 `execve` 得到 `ETXTBSY`，过几毫秒子进程 exec 走、fd 一关就好了。Go 的 `os/exec`
//! 与 Rust 社区的通行做法都是短暂退避后重试，这里照做。
//!
//! 只该包在"刚写完就执行"的地方（`komo update` 的试跑、测试里现写的假 worker）；执行一份
//! 早就落了盘的二进制不会遇到它，不必绕这一层。

use std::io;
use std::time::Duration;

/// 最多试几次（含第一次）。
const ATTEMPTS: u32 = 10;
/// 第一次退避；之后每次翻倍，直到 [`CAP`]。
const FIRST_DELAY: Duration = Duration::from_millis(2);
/// 单次退避的上限。总等待 2+4+8+16+32+64+100+100+100 ≈ 0.43s。
const CAP: Duration = Duration::from_millis(100);

/// 跑 `op`（通常是一次 `Command::spawn` / `output` / `status`），遇到 `ETXTBSY` 就退避重试，
/// 其余错误与成功原样返回；重试用尽后返回最后一次的 `ETXTBSY`。
///
/// 阻塞地睡：调用点本来就在同步地等子进程。
pub fn retry_text_file_busy<T>(op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_with(op, ATTEMPTS, FIRST_DELAY, CAP)
}

fn retry_with<T>(
    mut op: impl FnMut() -> io::Result<T>,
    attempts: u32,
    first_delay: Duration,
    cap: Duration,
) -> io::Result<T> {
    let mut delay = first_delay;
    let mut attempt = 1;
    loop {
        match op() {
            Err(error)
                if error.kind() == io::ErrorKind::ExecutableFileBusy && attempt < attempts =>
            {
                std::thread::sleep(delay);
                delay = (delay * 2).min(cap);
                attempt += 1;
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy() -> io::Error {
        io::Error::from(io::ErrorKind::ExecutableFileBusy)
    }

    const TINY: Duration = Duration::from_micros(10);

    /// 前几次 `ETXTBSY`、之后成功：重试到成功为止，不多跑。
    #[test]
    fn busy_then_ok_is_retried_until_it_succeeds() {
        let mut calls = 0;
        let result = retry_with(
            || {
                calls += 1;
                if calls <= 3 { Err(busy()) } else { Ok(calls) }
            },
            ATTEMPTS,
            TINY,
            TINY,
        );
        assert_eq!(result.unwrap(), 4);
    }

    /// 一直 `ETXTBSY`：试满次数就停，错误原样交回。
    #[test]
    fn a_file_that_stays_busy_gives_up_after_the_attempt_limit() {
        let mut calls = 0;
        let error = retry_with(
            || -> io::Result<()> {
                calls += 1;
                Err(busy())
            },
            ATTEMPTS,
            TINY,
            TINY,
        )
        .unwrap_err();
        assert_eq!(calls, ATTEMPTS);
        assert_eq!(error.kind(), io::ErrorKind::ExecutableFileBusy);
    }

    /// 别的错误（找不到、没权限）不是这个竞态：一次就返回，不白等。
    #[test]
    fn other_errors_are_not_retried() {
        let mut calls = 0;
        let error = retry_with(
            || -> io::Result<()> {
                calls += 1;
                Err(io::Error::from(io::ErrorKind::NotFound))
            },
            ATTEMPTS,
            TINY,
            TINY,
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    /// 内核真给的 errno 26 要被认成 `ExecutableFileBusy`——重试靠的就是这个映射。
    #[cfg(unix)]
    #[test]
    fn the_real_errno_maps_to_the_busy_kind() {
        let error = io::Error::from_raw_os_error(26);
        assert_eq!(error.kind(), io::ErrorKind::ExecutableFileBusy);
    }
}
