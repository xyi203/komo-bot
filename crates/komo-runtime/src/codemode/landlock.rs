//! Linux 的 codemode 沙箱（`docs/codemode.md` §4）：Landlock + seccomp，在 `pre_exec` 里装上。
//!
//! - **Landlock**：文件只读白名单（解释器的安装目录 + 动态库目录），只许执行解释器本身与它的
//!   ELF 加载器，写只放 `/dev/null`；ABI ≥ 4 时 TCP 连接 / 监听一并拒，ABI ≥ 6 时信号与抽象
//!   Unix socket 不出沙箱。
//! - **seccomp**：`socket` / `socketpair` 直接 `EPERM`（Landlock 管不到 UDP 与 Unix socket），
//!   外加几条能绕过上面这些的系统调用。
//!
//! 规则集与过滤器在父进程里先建好（要分配内存、开文件），子进程 fork 之后只做三个系统调用：
//! `PR_SET_NO_NEW_PRIVS`、`landlock_restrict_self`、`seccomp`——`pre_exec` 里不分配。
//!
//! 结构体与常量照抄内核 uapi（`linux/landlock.h`、`linux/seccomp.h`、`linux/filter.h`）；
//! libc 只给了系统调用号与 `sock_filter`。

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::Paths;

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
/// ABI 1 的十三种文件操作（执行、读写、删、建各类节点）。
const FS_ABI1: u64 = (1 << 13) - 1;
const FS_REFER: u64 = 1 << 13; // ABI 2
const FS_TRUNCATE: u64 = 1 << 14; // ABI 3
const FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5
/// 能挂在单个文件（不是目录）上的那几种。
const FS_FILE_ONLY: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

const NET_BIND_TCP: u64 = 1 << 0; // ABI 4
const NET_CONNECT_TCP: u64 = 1 << 1;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0; // ABI 6
const SCOPE_SIGNAL: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// `seccomp` 里拒掉的系统调用：网络、能绕开 Landlock 的 io_uring、读写别的进程的内存、
/// 内核里跑程序。全是 `EPERM`，Python 那边就是一个普通的 `PermissionError`。
///
/// 新 socket 开不出来；`connect` 一类再拒一遍，是防万一继承下来一个 socket fd（与 Grok Build
/// 的子进程网络过滤器同一份名单）。Codex 放弃 Landlock 的理由——它拦不住对已有 Unix socket
/// 的 `connect`——在这里由这两层兜住。
const DENIED_SYSCALLS: &[libc::c_long] = &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_sendto,
    libc::SYS_sendmsg,
    libc::SYS_sendmmsg,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
];

/// 一份建好的约束：Landlock 规则集（fd）+ seccomp 过滤器。可以反复用在每一次脚本上。
#[derive(Debug, Clone)]
pub(super) struct Jail {
    ruleset: Arc<OwnedFd>,
    filter: Arc<Vec<libc::sock_filter>>,
}

impl Jail {
    pub(super) fn new(paths: &Paths) -> Result<Jail, String> {
        let abi =
            abi().ok_or("这台机器的内核没有开 Landlock（要 5.13+ 且 LSM 列表里有 landlock）")?;
        let handled_fs = FS_ABI1
            | if abi >= 2 { FS_REFER } else { 0 }
            | if abi >= 3 { FS_TRUNCATE } else { 0 }
            | if abi >= 5 { FS_IOCTL_DEV } else { 0 };
        let attr = RulesetAttr {
            handled_access_fs: handled_fs,
            handled_access_net: if abi >= 4 {
                NET_BIND_TCP | NET_CONNECT_TCP
            } else {
                0
            },
            scoped: if abi >= 6 {
                SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
            } else {
                0
            },
        };
        // SAFETY: attr 是按内核布局排的结构体，大小如实给出；返回的是新 fd 或 -1。
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(format!(
                "Landlock 规则集建不起来：{}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: 刚拿到的 fd 只归这里（内核给的就是 O_CLOEXEC）。
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };

        let real = std::fs::canonicalize(&paths.executable)
            .map_err(|error| format!("{} 找不到真实路径：{error}", paths.executable.display()))?;
        let mut executables = vec![real.clone()];
        if let Some(loader) = elf_interpreter(&real)? {
            executables.push(std::fs::canonicalize(&loader).unwrap_or(loader));
        }
        for path in &executables {
            allow(&ruleset, path, (FS_EXECUTE | FS_READ_FILE) & handled_fs)?;
        }
        for path in read_roots(paths) {
            if path.exists() {
                allow(&ruleset, &path, (FS_READ_FILE | FS_READ_DIR) & handled_fs)?;
            }
        }
        allow(
            &ruleset,
            Path::new("/dev/null"),
            (FS_READ_FILE | FS_WRITE_FILE) & handled_fs,
        )?;

        Ok(Jail {
            ruleset: Arc::new(ruleset),
            filter: Arc::new(seccomp_filter()),
        })
    }

    /// 起解释器的命令：fork 之后、exec 之前把自己关进去。
    pub(super) fn command(&self, interpreter: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(interpreter);
        let ruleset = self.ruleset.as_raw_fd();
        let filter = Arc::clone(&self.filter);
        // SAFETY: 闭包只做系统调用，不分配、不加锁；`filter` 与 `ruleset` 由 `self`（经
        // `Arc`）活过这次 spawn。
        unsafe {
            command.pre_exec(move || {
                let program = libc::sock_fprog {
                    len: filter.len() as libc::c_ushort,
                    filter: filter.as_ptr() as *mut libc::sock_filter,
                };
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    0u32,
                    &program as *const libc::sock_fprog,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }
}

/// Landlock 的 ABI 版本；`None` = 内核没有或没开。
pub(super) fn abi() -> Option<i64> {
    // SAFETY: 查版本的固定用法：空指针、大小 0、带 VERSION 标志。
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    (abi >= 1).then_some(abi)
}

/// 只读的根：解释器的安装目录（托管 venv 的 `site-packages` 在 `prefix` 下）与动态库。
fn read_roots(paths: &Paths) -> Vec<PathBuf> {
    let mut roots = vec![paths.prefix.clone(), paths.base_prefix.clone()];
    roots.extend(
        [
            "/lib",
            "/lib64",
            "/usr/lib",
            "/usr/lib64",
            "/etc/ld.so.cache",
            "/dev/urandom",
        ]
        .map(PathBuf::from),
    );
    roots
}

fn allow(ruleset: &OwnedFd, path: &Path, access: u64) -> Result<(), String> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{} 里有 NUL", path.display()))?;
    // SAFETY: 以 O_PATH 打开，只用来指认这一处；下面立刻包进 OwnedFd。
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "{} 打不开：{}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: 刚打开的 fd 只归这里。
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let access = if path.is_dir() {
        access
    } else {
        access & FS_FILE_ONLY
    };
    let rule = PathBeneathAttr {
        allowed_access: access,
        parent_fd: fd.as_raw_fd(),
    };
    // SAFETY: rule 按内核的 packed 布局排，两个 fd 都活着。
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            RULE_PATH_BENEATH,
            &rule as *const PathBeneathAttr,
            0u32,
        )
    };
    if result != 0 {
        return Err(format!(
            "Landlock 规则加不上（{}）：{}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// 动态链接的解释器 exec 时内核还要打开它的加载器（`PT_INTERP`，如
/// `/lib64/ld-linux-x86-64.so.2`），那一下也要 Landlock 的执行权。静态链接的没有。
fn elf_interpreter(executable: &Path) -> Result<Option<PathBuf>, String> {
    const PT_INTERP: u32 = 3;
    let bytes = std::fs::read(executable)
        .map_err(|error| format!("{} 读不了：{error}", executable.display()))?;
    let u16_at = |at: usize| {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let u64_at = |at: usize| {
        bytes
            .get(at..at + 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()) as usize)
    };
    // ELF64、小端（komo 发布的两个 Linux 目标都是）。
    if bytes.get(..6) != Some(&[0x7f, b'E', b'L', b'F', 2, 1][..]) {
        return Err(format!("{} 不是 64 位小端 ELF", executable.display()));
    }
    let malformed = || format!("{} 的 ELF 头读不懂", executable.display());
    let phoff = u64_at(0x20).ok_or_else(malformed)?;
    let phentsize = u16_at(0x36).ok_or_else(malformed)? as usize;
    let phnum = u16_at(0x38).ok_or_else(malformed)? as usize;
    for index in 0..phnum {
        let header = phoff + index * phentsize;
        if u32_at(header).ok_or_else(malformed)? != PT_INTERP {
            continue;
        }
        let offset = u64_at(header + 0x08).ok_or_else(malformed)?;
        let size = u64_at(header + 0x20).ok_or_else(malformed)?;
        let raw = bytes.get(offset..offset + size).ok_or_else(malformed)?;
        let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
        return Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(raw))));
    }
    Ok(None)
}

/// 架构不对就杀进程（防 32 位系统调用表绕过）；x86_64 上 x32 的调用号一律拒；
/// 名单里的 `EPERM`；其余放行。
fn seccomp_filter() -> Vec<libc::sock_filter> {
    const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
    const OFFSET_NR: u32 = 0;
    const OFFSET_ARCH: u32 = 4;
    let stmt = |code: u16, k: u32| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code: u16, k: u32, jt: u8, jf: u8| libc::sock_filter { code, jt, jf, k };
    let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;

    let mut program = vec![
        stmt(LD_W_ABS, OFFSET_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, libc::SECCOMP_RET_KILL_PROCESS),
        stmt(LD_W_ABS, OFFSET_NR),
    ];
    let count = DENIED_SYSCALLS.len();
    if cfg!(target_arch = "x86_64") {
        // x32 ABI：调用号带 0x40000000 位，同一个系统调用换个号就绕过名单。
        const JGE_K: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
        program.push(jump(JGE_K, 0x4000_0000, count as u8 + 1, 0));
    }
    for (index, nr) in DENIED_SYSCALLS.iter().enumerate() {
        // 命中跳到最后一条（拒），没命中往下比。
        program.push(jump(JEQ_K, *nr as u32, (count - index) as u8, 0));
    }
    program.push(stmt(RET_K, libc::SECCOMP_RET_ALLOW));
    program.push(stmt(RET_K, deny));
    program
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_python_loader_is_read_from_the_elf_header() {
        let Ok(python) = std::fs::canonicalize("/usr/bin/python3") else {
            return;
        };
        let loader = elf_interpreter(&python)
            .unwrap()
            .expect("系统 python 是动态链接的");
        assert!(loader.to_string_lossy().contains("ld-linux"), "{loader:?}");
    }

    #[test]
    fn every_denied_syscall_jumps_to_the_deny_return() {
        let program = seccomp_filter();
        let deny = program.len() - 1;
        for (index, op) in program.iter().enumerate() {
            if DENIED_SYSCALLS.contains(&(op.k as libc::c_long))
                && op.code == (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16
            {
                assert_eq!(index + 1 + op.jt as usize, deny);
            }
        }
    }
}
