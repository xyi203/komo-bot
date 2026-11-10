//! 子进程的登记与核对：`bash` 转后台、监听脚本留下的进程，Gateway 重启后靠进程身份认回
//! （`docs/komo_bot.md` §8.7）。
//!
//! 旧的启动扫描与账本核对（`RecoveryScan` / 决策表）随旧存储删除；新存储的恢复在
//! `durable::harness::recovery`。

mod processes;

pub use processes::{
    ChildProcess, ChildRegistry, IdentityProbe, Liveness, ProcessProbe, SysProcessProbe,
};
