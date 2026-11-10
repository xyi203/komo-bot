//! 遗留子进程的登记与核实（§8.7）。
//!
//! 「异常退出可能遗留子进程，不能仅凭一个 PID 判断是否为原进程；结合启动身份和进程启动
//! 时刻核对。」Gateway 对数据目录持进程锁（§3），拿到锁就说明上一个 Gateway 进程不在了；
//! 锁管不到 shell 留下的进程组，所以谁启动它们谁在 [`ChildRegistry`] 里留一行（pid / 进程组 /
//! 启动时刻），[`ProcessProbe`] 逐行核实。

use std::path::{Path, PathBuf};

use komo_kernel::types::ids::ExecutorId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 在册的一个子进程。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildProcess {
    pub pid: u32,
    /// 进程组——取消时要终止的是它（§4）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgid: Option<u32>,
    /// 这个 pid 是什么时候起来的。**pid 会被复用**，所以身份要靠它核对。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<OffsetDateTime>,
    /// 给操作者看的一句话。
    #[serde(default)]
    pub what: String,
}

/// 一个 pid 现在的样子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// 确定没了。
    Gone,
    /// 还在。
    Alive,
    /// 查不出来。**按"还在"处理**——阻止重复启动比重复执行安全。
    Unknown,
}

/// 探一个 pid。
pub trait ProcessProbe: Send + Sync + std::fmt::Debug {
    fn probe(&self, child: &ChildProcess) -> Liveness;
}

/// 系统探测：Linux 读 `/proc/<pid>`，其他平台用 `ps -p`。
#[derive(Debug, Clone, Copy, Default)]
pub struct SysProcessProbe;

impl ProcessProbe for SysProcessProbe {
    fn probe(&self, child: &ChildProcess) -> Liveness {
        #[cfg(target_os = "linux")]
        {
            let path = PathBuf::from(format!("/proc/{}", child.pid));
            if !path.exists() {
                return Liveness::Gone;
            }
            // pid 复用：`/proc/<pid>/stat` 的第 22 个字段是启动时刻（时钟嘀嗒数）。
            // 我们没有记 btime 与 HZ，换算不了成绝对时间，所以这里只能答"还在"——
            // 而"还在"正是保守的那一边。
            Liveness::Alive
        }
        #[cfg(not(target_os = "linux"))]
        {
            match std::process::Command::new("ps")
                .args(["-p", &child.pid.to_string()])
                .output()
            {
                Ok(output) if output.status.success() => Liveness::Alive,
                Ok(_) => Liveness::Gone,
                Err(_) => Liveness::Unknown,
            }
        }
    }
}

/// 带身份核对的探测（新存储的 `probe` 阶段用，`docs/storage-durable.md` §5）。
///
/// 和 [`SysProcessProbe`] 的区别：登记行带了 `started_at` 时，**进程起来的时刻对不上就是
/// "不是那个进程"**（pid 被复用了），按 `Gone` 处理；僵尸进程（已退出、等人收尸）也是 `Gone`。
/// 没带 `started_at` 就只能看 pid 在不在。读不出来的一律 `Unknown`——保守地当它还在。
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityProbe;

/// 起来的时刻与登记行相差多少秒以内算同一个进程（时钟嘀嗒与墙钟换算有几秒误差）。
const START_TOLERANCE_SECS: i64 = 10;

/// 一个活着的进程此刻的样子。
struct Observed {
    zombie: bool,
    /// 起来的时刻（Unix 秒）；读不出来为 `None`。
    started_at: Option<i64>,
}

impl ProcessProbe for IdentityProbe {
    fn probe(&self, child: &ChildProcess) -> Liveness {
        let observed = match observe(child.pid) {
            Ok(Some(observed)) => observed,
            Ok(None) => return Liveness::Gone,
            Err(()) => return Liveness::Unknown,
        };
        if observed.zombie {
            return Liveness::Gone;
        }
        match (child.started_at, observed.started_at) {
            (Some(recorded), Some(actual)) => {
                if (recorded.unix_timestamp() - actual).abs() <= START_TOLERANCE_SECS {
                    Liveness::Alive
                } else {
                    Liveness::Gone
                }
            }
            // 登记了起来的时刻、这台机器上却读不出来：核实不了，当它还在。
            (Some(_), None) => Liveness::Unknown,
            // 登记行没有起来的时刻：只能说"这个 pid 还在"。
            (None, _) => Liveness::Alive,
        }
    }
}

/// `Ok(None)` = 没有这个 pid；`Err` = 查不出来。
#[cfg(target_os = "linux")]
fn observe(pid: u32) -> Result<Option<Observed>, ()> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    // `pid (comm) S ppid ...`：comm 里可以有空格和括号，所以从**最后一个** `)` 切。
    let rest = stat.rsplit_once(')').map(|(_, rest)| rest).ok_or(())?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let zombie = matches!(fields.first().copied(), Some("Z" | "X"));
    // 第 22 个字段 starttime（开机后的时钟嘀嗒数）：切掉 pid、comm 之后下标是 19。
    let started_at = fields
        .get(19)
        .and_then(|ticks| ticks.parse::<u64>().ok())
        .and_then(|ticks| {
            let boot = std::fs::read_to_string("/proc/stat")
                .ok()?
                .lines()
                .find_map(|line| line.strip_prefix("btime ")?.trim().parse::<i64>().ok())?;
            Some(boot + (ticks / clock_ticks_per_second()) as i64)
        });
    Ok(Some(Observed { zombie, started_at }))
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> u64 {
    // SAFETY: `sysconf` 只读系统常量。
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 { hz as u64 } else { 100 }
}

#[cfg(not(target_os = "linux"))]
fn observe(pid: u32) -> Result<Option<Observed>, ()> {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=,etime=", "-p", &pid.to_string()])
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let Some(stat) = parts.next() else {
        return Ok(None);
    };
    let started_at = parts
        .next()
        .and_then(parse_etime)
        .map(|elapsed| time::OffsetDateTime::now_utc().unix_timestamp() - elapsed as i64);
    Ok(Some(Observed {
        zombie: stat.starts_with('Z'),
        started_at,
    }))
}

/// `ps -o etime=` 的 `[[dd-]hh:]mm:ss`。
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_etime(text: &str) -> Option<u64> {
    let text = text.trim();
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut seconds = 0u64;
    let parts: Vec<&str> = clock.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    for part in parts {
        seconds = seconds * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + seconds)
}

/// `runtime/children/<executor>.json`：一个执行实例留下的在册子进程。
#[derive(Debug, Clone)]
pub struct ChildRegistry {
    dir: PathBuf,
}

impl ChildRegistry {
    /// `dir` 一般是 `paths.runtime_dir/children`。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ChildRegistry { dir: dir.into() }
    }

    fn file(&self, executor: &ExecutorId) -> PathBuf {
        self.dir.join(format!("{executor}.json"))
    }

    /// 这个执行实例名下在册的子进程。文件不在就是"一个都没有"。
    pub fn children(&self, executor: &ExecutorId) -> Vec<ChildProcess> {
        let path = self.file(executor);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "子进程登记读不出来，按没有在册子进程处理");
            Vec::new()
        })
    }

    /// 记一行。**启动子进程的那一方调它**（工具层），恢复扫描只读。
    pub fn record(&self, executor: &ExecutorId, child: ChildProcess) -> std::io::Result<()> {
        let mut children = self.children(executor);
        children.retain(|existing| existing.pid != child.pid);
        children.push(child);
        self.write(executor, &children)
    }

    /// 销掉一行（进程正常收尾了）。
    pub fn forget(&self, executor: &ExecutorId, pid: u32) -> std::io::Result<()> {
        let mut children = self.children(executor);
        children.retain(|existing| existing.pid != pid);
        self.write(executor, &children)
    }

    fn write(&self, executor: &ExecutorId, children: &[ChildProcess]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let text = serde_json::to_string_pretty(children)?;
        std::fs::write(self.file(executor), text)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(pid: u32) -> ChildProcess {
        ChildProcess {
            pid,
            pgid: Some(pid),
            started_at: None,
            what: "shell: cargo test".into(),
        }
    }

    #[test]
    fn a_child_that_finished_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ChildRegistry::new(dir.path());
        let old = ExecutorId::from_raw("exec-old");
        registry.record(&old, child(1)).unwrap();
        registry.record(&old, child(2)).unwrap();
        registry.forget(&old, 1).unwrap();
        assert_eq!(
            registry
                .children(&old)
                .into_iter()
                .map(|c| c.pid)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn the_system_probe_finds_this_very_process_alive() {
        let probe = SysProcessProbe;
        assert_eq!(probe.probe(&child(std::process::id())), Liveness::Alive);
    }

    #[test]
    fn a_corrupt_registry_file_reads_as_no_children_rather_than_failing_startup() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ChildRegistry::new(dir.path());
        std::fs::write(dir.path().join("exec-old.json"), "{ 半行").unwrap();
        assert!(
            registry
                .children(&ExecutorId::from_raw("exec-old"))
                .is_empty()
        );
    }

    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep")
    }

    fn recorded(pid: u32, started_at: Option<OffsetDateTime>) -> ChildProcess {
        ChildProcess {
            pid,
            pgid: Some(pid),
            started_at,
            what: "test".into(),
        }
    }

    #[test]
    fn the_identity_probe_accepts_a_matching_start_time_and_rejects_a_reused_pid() {
        let mut child = sleeper();
        let pid = child.id();
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            IdentityProbe.probe(&recorded(pid, Some(now))),
            Liveness::Alive,
            "the recorded start matches the live process"
        );
        assert_eq!(
            IdentityProbe.probe(&recorded(pid, None)),
            Liveness::Alive,
            "no start time recorded: the pid alone"
        );
        assert_eq!(
            IdentityProbe.probe(&recorded(pid, Some(now - time::Duration::hours(1)))),
            Liveness::Gone,
            "same pid, different start time: not the process we recorded"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(
            IdentityProbe.probe(&recorded(pid, Some(now))),
            Liveness::Gone,
            "a reaped process is gone"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_identity_probe_treats_a_zombie_as_gone() {
        let mut child = sleeper();
        let pid = child.id();
        let now = OffsetDateTime::now_utc();
        // 杀掉但不 wait：留下僵尸。
        // SAFETY: 给自己刚起的子进程发 SIGKILL。
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        for _ in 0..200 {
            if IdentityProbe.probe(&recorded(pid, Some(now))) == Liveness::Gone {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            IdentityProbe.probe(&recorded(pid, Some(now))),
            Liveness::Gone
        );
        child.wait().unwrap();
    }

    #[test]
    fn etime_parses_every_ps_shape() {
        assert_eq!(parse_etime("05:09"), Some(309));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-01:02:03"), Some(2 * 86_400 + 3723));
        assert_eq!(parse_etime("  "), None);
    }
}
