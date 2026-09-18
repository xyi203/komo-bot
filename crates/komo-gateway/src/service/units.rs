//! 后台进程：Fedora 用 systemd --user，Mac 用 launchd（§3）。
//!
//! 「服务管理器运行**前台形式**的 Gateway」——单元里写的永远是
//! `komo gateway --foreground`；`komo gateway` 自己只负责把单元装上并请服务管理器起它，
//! 然后等就绪。
//!
//! 「`status` 与 `stop` **不隐式启动服务**。」
//!
//! **单元里只有 `KOMO_HOME`，没有任何凭证。** `.env` 由 Gateway 自己读（§12），要用的
//! 时候按名解析（`service::python_env` 的 `ToolboxSecrets`）。理由有三条，每一条单独
//! 就够：launchd 没有 `EnvironmentFile` 的等价物，只能把值逐字抄进 plist；一旦抄进去，
//! `.env` 的热重载就失效了，旧值活到下一次重装单元为止；而进了进程环境的东西，这台
//! 机器上每一个子进程与每一条 `/proc/<pid>/environ` 都看得见。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 单元 / 作业的名字。
pub const UNIT: &str = "komo-gateway";
/// launchd 的标签。
pub const LABEL: &str = "dev.komo.gateway";

#[derive(Debug, thiserror::Error)]
pub enum UnitError {
    /// 这台机器上没有服务管理器（容器、或者没跑 systemd 的 Linux）。
    #[error("这台机器上没有可用的服务管理器：请前台运行 `komo gateway --foreground`")]
    NoManager,
    #[error("写单元文件 {path} 失败：{message}")]
    Write { path: PathBuf, message: String },
    #[error("{command} 失败：{message}")]
    Command { command: String, message: String },
}

/// 这台机器用哪一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Launchd,
    Systemd,
    None,
}

pub fn manager() -> Manager {
    if cfg!(target_os = "macos") {
        return Manager::Launchd;
    }
    // systemd --user 要有一个用户总线才谈得上（容器里通常没有）。
    if cfg!(target_os = "linux") && which("systemctl").is_some() && user_bus_present() {
        return Manager::Systemd;
    }
    Manager::None
}

fn user_bus_present() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        || std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|dir| Path::new(&dir).join("systemd/private").exists())
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// 单元文件的位置。
pub fn unit_path(home_dir: &Path) -> PathBuf {
    match manager() {
        Manager::Launchd => home_dir.join(format!("Library/LaunchAgents/{LABEL}.plist")),
        _ => home_dir.join(format!(".config/systemd/user/{UNIT}.service")),
    }
}

/// 单元文件的正文。
pub fn unit_text(manager: Manager, exe: &Path, komo_home: &Path, logs: &Path) -> String {
    match manager {
        Manager::Launchd => format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>gateway</string>
    <string>--foreground</string>
  </array>
  <!-- 只有 KOMO_HOME。凭证在 {home}/.env，由 Gateway 自己读取，不进本文件。 -->
  <key>EnvironmentVariables</key>
  <dict><key>KOMO_HOME</key><string>{home}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{logs}/gateway.out.log</string>
  <key>StandardErrorPath</key><string>{logs}/gateway.err.log</string>
</dict>
</plist>
"#,
            exe = exe.display(),
            home = komo_home.display(),
            logs = logs.display(),
        ),
        _ => format!(
            r#"[Unit]
Description=komo gateway
After=network.target

[Service]
Type=simple
# 只有 KOMO_HOME。凭证在 {home}/.env，由 Gateway 自己读取，不进本文件。
Environment=KOMO_HOME={home}
ExecStart={exe} gateway --foreground
Restart=on-failure
RestartSec=3
# 停机有界：`Running::stop` 自己的排空窗口是 10s，再久就是这个进程卡住了。systemd 默认
# 停等 90s，真卡住一次 `komo gateway restart` 就白等一分半，所以这里压到 20s——超过就
# 让 systemd 收尾。
TimeoutStopSec=20

[Install]
WantedBy=default.target
"#,
            exe = exe.display(),
            home = komo_home.display(),
        ),
    }
}

/// 把单元文件写到用户目录。
pub fn install(home_dir: &Path, komo_home: &Path) -> Result<PathBuf, UnitError> {
    let manager = manager();
    if manager == Manager::None {
        return Err(UnitError::NoManager);
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("komo"));
    let logs = komo_home.join("logs");
    let _ = std::fs::create_dir_all(&logs);
    let path = unit_path(home_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| UnitError::Write {
            path: path.clone(),
            message: error.to_string(),
        })?;
    }
    std::fs::write(&path, unit_text(manager, &exe, komo_home, &logs)).map_err(|error| {
        UnitError::Write {
            path: path.clone(),
            message: error.to_string(),
        }
    })?;
    Ok(path)
}

/// 请服务管理器起它。
pub fn start(home_dir: &Path, komo_home: &Path) -> Result<(), UnitError> {
    let path = install(home_dir, komo_home)?;
    match manager() {
        Manager::Launchd => {
            // `bootstrap` 之后再 `kickstart`：已经装过的那一次 bootstrap 会失败，忽略。
            let domain = format!("gui/{}", uid());
            let _ = run(
                "launchctl",
                &["bootstrap", &domain, &path.display().to_string()],
            );
            run("launchctl", &["kickstart", &format!("{domain}/{LABEL}")])
        }
        Manager::Systemd => {
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", "--now", UNIT])
        }
        Manager::None => Err(UnitError::NoManager),
    }
}

/// 停。**不隐式启动服务。**
pub fn stop() -> Result<(), UnitError> {
    match manager() {
        Manager::Launchd => run("launchctl", &["bootout", &format!("gui/{}/{LABEL}", uid())]),
        Manager::Systemd => run("systemctl", &["--user", "stop", UNIT]),
        Manager::None => Err(UnitError::NoManager),
    }
}

/// 重启。
pub fn restart(home_dir: &Path, komo_home: &Path) -> Result<(), UnitError> {
    let _ = stop();
    // launchd 的 bootout 是异步的：服务还在卸的那几百毫秒里 bootstrap 会失败（被
    // `start` 吞掉），随后 kickstart 就找不到服务。等它真的消失再起。
    for _ in 0..25 {
        if status().is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    start(home_dir, komo_home)
}

/// 服务管理器眼里它是什么状态。**不隐式启动。**
pub fn status() -> Result<String, UnitError> {
    match manager() {
        Manager::Launchd => output("launchctl", &["print", &format!("gui/{}/{LABEL}", uid())]),
        Manager::Systemd => output("systemctl", &["--user", "is-active", UNIT]),
        Manager::None => Err(UnitError::NoManager),
    }
}

fn uid() -> u32 {
    // `id -u` 而不是 libc：依赖清单里没有 libc（§13.4）。
    output("id", &["-u"])
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(501)
}

fn run(program: &str, args: &[&str]) -> Result<(), UnitError> {
    output(program, args).map(|_| ())
}

fn output(program: &str, args: &[&str]) -> Result<String, UnitError> {
    let result = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| UnitError::Command {
            command: format!("{program} {}", args.join(" ")),
            message: error.to_string(),
        })?;
    let stdout = String::from_utf8_lossy(&result.stdout).to_string();
    if result.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    Err(UnitError::Command {
        command: format!("{program} {}", args.join(" ")),
        message: if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_systemd_unit_runs_the_foreground_form() {
        let text = unit_text(
            Manager::Systemd,
            Path::new("/usr/local/bin/komo"),
            Path::new("/home/u/.komo"),
            Path::new("/home/u/.komo/logs"),
        );
        assert!(
            text.contains("ExecStart=/usr/local/bin/komo gateway --foreground"),
            "{text}"
        );
        assert!(text.contains("KOMO_HOME=/home/u/.komo"), "{text}");
    }

    /// **单元文件里一个凭证都没有**（§12：`.env` 由 Gateway 自己读）。
    ///
    /// 断言的是"没有"，所以要拿真的会出现在 `.env` 里的那几个名字去比——一个只检查
    /// "不含 `.env` 三个字"的测试挡不住任何东西。
    #[test]
    fn no_unit_file_carries_anything_from_dot_env() {
        // 假的 `.env` 内容：名字与值都用真实形状。
        let secrets = [
            ("KOMO_LLM_API_KEY", "sk-live-0123456789abcdef"),
            ("TELEGRAM_BOT_TOKEN", "1234567:AAH-live-token"),
            ("MEMOS_TOKEN", "eyJhbGciOiJIUzI1NiJ9.live"),
            ("FEISHU_APP_SECRET", "live-app-secret"),
        ];
        for manager in [Manager::Systemd, Manager::Launchd] {
            let text = unit_text(
                manager,
                Path::new("/usr/local/bin/komo"),
                Path::new("/home/u/.komo"),
                Path::new("/home/u/.komo/logs"),
            );
            for (name, value) in secrets {
                assert!(
                    !text.contains(value),
                    "{manager:?} 的单元里出现了凭证：{text}"
                );
                assert!(
                    !text.contains(name),
                    "{manager:?} 的单元里连凭证的变量名都不该有：{text}"
                );
            }
            // 唯一该有的那个环境变量，以及那句说明。
            assert!(text.contains("KOMO_HOME"), "{text}");
            assert_eq!(
                text.matches("KOMO_HOME").count(),
                2,
                "一次设置 + 一次注释里提到它，再多就是别处又设了一遍：{text}"
            );
            assert!(
                text.contains("/.env，由 Gateway 自己读取，不进本文件"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_launchd_job_runs_the_foreground_form_too() {
        let text = unit_text(
            Manager::Launchd,
            Path::new("/usr/local/bin/komo"),
            Path::new("/Users/u/.komo"),
            Path::new("/Users/u/.komo/logs"),
        );
        assert!(text.contains("<string>--foreground</string>"), "{text}");
        assert!(text.contains(LABEL), "{text}");
    }
}
