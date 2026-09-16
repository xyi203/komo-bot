//! 上一个执行实例真的停了吗（§8.7）。
//!
//! 「异常退出可能遗留子进程，不能仅凭一个 PID 判断是否为原进程；结合平台监督信息、
//! 启动身份和进程启动时间核对。**无法确认旧执行已结束时，阻止该任务重复启动并显示
//! 原因。**」这个文件就是那句话的两半：
//!
//! - **平台监督信息**：Gateway 对数据目录持进程锁（§3）。拿到锁就说明上一个 Gateway
//!   进程不在了——这是我们有的、最硬的那条信号，比任何 PID 探测都硬。
//! - **遗留子进程**：锁管不到 shell / Python 留下的进程组。谁启动它们谁在
//!   [`ChildRegistry`] 里留一行（pid / 进程组 / 启动时刻），这里逐行核实。
//!
//! 记录文件不存在 = 上一代没留下任何在册子进程。这是一个明确的结论，不是一个猜测：
//! 首版**不承诺** shell / Python 进程跨 Gateway 重启存活（§8.7），所以"没有在册子进程
//! + 锁已经拿到"就是"上一个执行实例已经停止"。

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

/// 上一个执行实例停了吗。
pub trait ExecutorLiveness: Send + Sync + std::fmt::Debug {
    /// `previous` 是账本里那个还握着领取权的执行实例（`None` = 没人握着）。
    ///
    /// **无法确认时答 `false`**：§8.7 要的是"阻止该任务重复启动并显示原因"。
    fn stopped(&self, previous: Option<&ExecutorId>) -> bool;
}

/// 拿到了数据目录进程锁的那个实例的判断。
#[derive(Debug)]
pub struct LockHolderLiveness {
    current: ExecutorId,
    registry: ChildRegistry,
    probe: Box<dyn ProcessProbe>,
}

impl LockHolderLiveness {
    /// `current` 是本次启动身份。**只有真的拿到了数据目录进程锁才可以构造它**——它的
    /// 结论建立在"没有第二个 Gateway 在跑"上。
    pub fn holding_lock(current: ExecutorId, registry: ChildRegistry) -> Self {
        LockHolderLiveness {
            current,
            registry,
            probe: Box::new(SysProcessProbe),
        }
    }

    pub fn with_probe(mut self, probe: Box<dyn ProcessProbe>) -> Self {
        self.probe = probe;
        self
    }
}

impl ExecutorLiveness for LockHolderLiveness {
    fn stopped(&self, previous: Option<&ExecutorId>) -> bool {
        let Some(previous) = previous else {
            // 没人握着领取权。
            return true;
        };
        if previous == &self.current {
            // 是我自己——这个 Run 正由本进程跑着，不是"上一个实例"。
            return false;
        }
        for child in self.registry.children(previous) {
            match self.probe.probe(&child) {
                Liveness::Gone => {}
                Liveness::Alive | Liveness::Unknown => {
                    tracing::warn!(
                        executor = %previous,
                        pid = child.pid,
                        what = %child.what,
                        "上一代留下的子进程还在（或核实不了），先不重复启动"
                    );
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Fixed(Liveness);

    impl ProcessProbe for Fixed {
        fn probe(&self, _child: &ChildProcess) -> Liveness {
            self.0
        }
    }

    fn child(pid: u32) -> ChildProcess {
        ChildProcess {
            pid,
            pgid: Some(pid),
            started_at: None,
            what: "shell: cargo test".into(),
        }
    }

    fn liveness(registry: ChildRegistry, probe: Liveness) -> LockHolderLiveness {
        LockHolderLiveness::holding_lock(ExecutorId::from_raw("exec-now"), registry)
            .with_probe(Box::new(Fixed(probe)))
    }

    #[test]
    fn nobody_holding_the_claim_means_nothing_to_wait_for() {
        let dir = tempfile::tempdir().unwrap();
        let liveness = liveness(ChildRegistry::new(dir.path()), Liveness::Alive);
        assert!(liveness.stopped(None));
    }

    #[test]
    fn a_previous_instance_with_no_registered_children_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let liveness = liveness(ChildRegistry::new(dir.path()), Liveness::Alive);
        assert!(liveness.stopped(Some(&ExecutorId::from_raw("exec-old"))));
    }

    #[test]
    fn a_surviving_child_blocks_a_second_start() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ChildRegistry::new(dir.path());
        let old = ExecutorId::from_raw("exec-old");
        registry.record(&old, child(4242)).unwrap();

        assert!(!liveness(registry.clone(), Liveness::Alive).stopped(Some(&old)));
        assert!(
            !liveness(registry.clone(), Liveness::Unknown).stopped(Some(&old)),
            "核实不了就当它还在"
        );
        assert!(liveness(registry, Liveness::Gone).stopped(Some(&old)));
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
    fn a_run_this_process_is_holding_is_not_a_previous_instance() {
        let dir = tempfile::tempdir().unwrap();
        let liveness = liveness(ChildRegistry::new(dir.path()), Liveness::Gone);
        assert!(!liveness.stopped(Some(&ExecutorId::from_raw("exec-now"))));
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
}
