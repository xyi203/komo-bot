//! 进程资源上限：Gateway 起来时把"能开几个文件"的软上限抬到够用。
//!
//! launchd 给后台服务的默认软上限是 256（`launchctl limit maxfiles`），systemd 是 1024。
//! shell / python 的子进程继承 Gateway 的上限，于是 `cargo test --workspace` 这类一次
//! 开几百个文件的命令在 komo 里跑就失败，在终端里跑却好好的——实测会话里模型得自己
//! 摸索出 `ulimit -n 65536` 才跑得下去，白耗几轮。
//!
//! 只动软上限、不碰硬上限；抬到 `min(硬上限, TARGET)`。macOS 的硬上限常是 unlimited，
//! 但 `setrlimit` 不收超过 `kern.maxfilesperproc` 的值，所以从高到低试几档，第一档成功
//! 就停。抬不上去只记日志，不挡 Gateway 起来。

/// 抬到多少为止。够 `cargo test` 这一档用，又不至于把子进程里用 `select()` 的老程序
/// 推过 `FD_SETSIZE` 太远。
pub const TARGET: u64 = 65_536;

/// 抬一次的结果：原来的软上限与现在的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raised {
    pub before: u64,
    pub after: u64,
}

/// 按 `TARGET` 抬"能开的文件数"的软上限。已经够了就不动（`before == after`）。
#[cfg(unix)]
pub fn raise_open_files() -> std::io::Result<Raised> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit 只往传进去的结构里写。
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let before = limit.rlim_cur as u64;
    let mut last = None;
    for want in candidates(limit.rlim_max as u64) {
        if want <= before {
            break;
        }
        let next = libc::rlimit {
            rlim_cur: want as libc::rlim_t,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: setrlimit 只读传进去的结构；失败时上限不变。
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &next) } == 0 {
            return Ok(Raised {
                before,
                after: want,
            });
        }
        last = Some(std::io::Error::last_os_error());
    }
    match last {
        Some(error) => Err(error),
        None => Ok(Raised {
            before,
            after: before,
        }),
    }
}

#[cfg(not(unix))]
pub fn raise_open_files() -> std::io::Result<Raised> {
    Ok(Raised {
        before: 0,
        after: 0,
    })
}

/// 从高到低要试的几档：`min(硬上限, TARGET)`，再往下是 macOS `OPEN_MAX` 的 10240、
/// 4096。硬上限比哪一档都低时，它自己就是唯一一档。
fn candidates(hard: u64) -> Vec<u64> {
    let mut out = vec![hard.min(TARGET)];
    for step in [10_240, 4_096] {
        if step < out[out.len() - 1] {
            out.push(step);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_capped_by_the_hard_limit() {
        assert_eq!(candidates(1_024), vec![1_024]);
        assert_eq!(candidates(8_192), vec![8_192, 4_096]);
    }

    #[test]
    fn an_unlimited_hard_limit_falls_back_through_the_macos_steps() {
        assert_eq!(candidates(u64::MAX), vec![TARGET, 10_240, 4_096]);
    }

    #[cfg(unix)]
    #[test]
    fn raising_never_lowers_the_soft_limit() {
        let raised = raise_open_files().unwrap();
        assert!(raised.after >= raised.before, "{raised:?}");
    }
}
